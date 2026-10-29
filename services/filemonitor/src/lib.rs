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

pub mod doctor;
mod routes;
pub mod setup;

use std::future::Future;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use ascom_alpaca::api::{CargoServerInfo, Device, SafetyMonitor};
use ascom_alpaca::{ASCOMError, ASCOMErrorCode, ASCOMResult, Server, SetupRequest, SetupResponse};
use rusty_photon_config::driver::ConfigurableDriver;
use rusty_photon_config::ConfigFile;
pub use rusty_photon_server_config::AlpacaServerConfig;
use rusty_photon_service_lifecycle::ReloadSignal;
use rusty_photon_setup::SetupPages;
use rusty_photon_tls::config::TlsConfig;
use rusty_photon_tls::server::Draining;
use serde::{Deserialize, Serialize};
use tokio::net::TcpListener;
use tokio::sync::{Mutex, RwLock};
use tokio::time::{interval, Duration};
use tokio_util::sync::CancellationToken;
use tracing::{debug, info, warn};

use crate::routes::{Replaced, Routes};
use crate::setup::{FileMonitorDriver, DEVICE_TYPE};

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

impl ConfigFile for Config {
    /// The rules the setup page's save applies (`FileMonitorDriver::validate`),
    /// so a file the page would refuse does not start the service either:
    /// [`load_config`] applies them, and the startup bootstrap holds the
    /// scaffold it writes to them.
    fn check(&self) -> Result<(), String> {
        match <FileMonitorDriver as ConfigurableDriver>::validate(self)
            .into_iter()
            .next()
        {
            Some(error) => Err(format!("{} {}", error.path, error.msg)),
            None => Ok(()),
        }
    }
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

impl ParsingConfig {
    /// Compiles the regex a `regex`-type rule is matched with: the pattern
    /// as written, or under `(?i)` when matching is case-insensitive. The
    /// one compile behind the evaluation and the rule check at the load and
    /// on a setup-page save, so a pattern is checked as the regex it runs
    /// as — `(?i)` widens a character class, and a pattern that fits the
    /// regex size limit only as written would otherwise pass the check and
    /// then never match.
    ///
    /// # Errors
    ///
    /// The regex crate's error when the pattern does not compile that way.
    pub fn compile_regex(&self, rule: &ParsingRule) -> Result<regex::Regex, regex::Error> {
        if self.case_sensitive {
            regex::Regex::new(&rule.pattern)
        } else {
            regex::Regex::new(&format!("(?i){}", rule.pattern))
        }
    }
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
/// keeps its own — see the W2 note in `docs/plans/archive/windows-packaging.md`);
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
    /// The setup pages behind `/setup/v1/safetymonitor/0/setup`: `Some` when
    /// the driver was built with a config source (the normal path through
    /// `ServerBuilder`); `None` for focused unit-test devices that don't
    /// exercise the page.
    #[debug(skip)]
    pages: Option<Arc<SetupPages<FileMonitorDriver>>>,
}

impl FileMonitorDevice {
    #[must_use]
    pub fn new(config: Config) -> Self {
        Self {
            config,
            connected: Arc::new(RwLock::new(false)),
            last_content: Arc::new(Mutex::new(None)),
            polling_handle: Arc::new(Mutex::new(None)),
            pages: None,
        }
    }

    /// Attach the shared setup pages, so the device's setup page renders and
    /// saves the `device` section.
    #[must_use]
    pub fn with_setup_pages(mut self, pages: Arc<SetupPages<FileMonitorDriver>>) -> Self {
        self.pages = Some(pages);
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
                    // A rule whose regex does not compile never matches, so a
                    // file whose only rule it is reads unsafe. The rule check
                    // at the load compiles the same regex and refuses such a
                    // rule, so none reaches a running service.
                    self.config
                        .parsing
                        .compile_regex(rule)
                        .is_ok_and(|re| re.is_match(content))
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

    /// Disconnect: `is_safe` answers `NotConnected` and the polling stops.
    async fn disconnect(&self) {
        let mut conn_state = self.connected.write().await;
        *conn_state = false;
        drop(conn_state);
        self.stop_polling().await;
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
            self.disconnect().await;
        }

        Ok(())
    }

    async fn driver_info(&self) -> ASCOMResult<String> {
        Ok(self.config.device.description.clone())
    }

    async fn driver_version(&self) -> ASCOMResult<String> {
        Ok("0.1.0".to_string())
    }

    /// No custom action: the configuration moved to the setup pages.
    async fn supported_actions(&self) -> ASCOMResult<Vec<String>> {
        Ok(Vec::new())
    }

    /// The specification's answer to an unknown action name is
    /// `ActionNotImplemented`, not the generic `NotImplemented` the crate
    /// defaults to.
    async fn action(&self, action: String, _parameters: String) -> ASCOMResult<String> {
        Err(ASCOMError::new(
            ASCOMErrorCode::ACTION_NOT_IMPLEMENTED,
            format!("this device has no action {action:?}"),
        ))
    }

    /// The device's setup page, `/setup/v1/safetymonitor/0/setup`, for `GET`
    /// and `POST`, through the shared handler. A device built without the
    /// pages (a focused unit test) has none.
    fn setup_request(
        &self,
        request: SetupRequest,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = SetupResponse> + Send + '_>> {
        match &self.pages {
            Some(pages) => {
                rusty_photon_driver::device_setup_request(pages, DEVICE_TYPE, 0, request)
            }
            None => Box::pin(async {
                SetupResponse::text(
                    axum::http::StatusCode::NOT_FOUND,
                    "This device was built without its setup pages",
                )
            }),
        }
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

/// Load a [`Config`] from the JSON file at `path` and check it.
///
/// # Errors
///
/// Returns an error if the file cannot be read, does not parse as a
/// [`Config`], or breaks a rule the setup page's save applies
/// ([`ConfigFile::check`]): an empty `device.unique_id` or `file.path`, a
/// zero `file.polling_interval`, or a regex rule that does not compile.
pub fn load_config(path: &Path) -> Result<Config, Box<dyn std::error::Error + Send + Sync>> {
    let content = std::fs::read_to_string(path)?;
    let config: Config = serde_json::from_str(&content)?;
    config.check()?;
    Ok(config)
}

/// Builder for the ASCOM Alpaca filemonitor server.
///
/// The returned [`BoundServer`] can be inspected (e.g. `listen_addr()`)
/// before calling `start()`.
pub struct ServerBuilder {
    config: Config,
    /// Where a setup-page save persists and reload re-reads. `Some` enables
    /// the setup pages (together with `reload`).
    config_path: Option<PathBuf>,
    /// Reload trigger a setup-page save fires after its response flushes.
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

    /// Set the config source (persist path) the setup pages save to. Together
    /// with [`Self::with_reload_signal`], this enables the pages.
    #[must_use]
    pub fn with_config_source(mut self, path: PathBuf) -> Self {
        self.config_path = Some(path);
        self
    }

    /// Provide the reload trigger a setup-page save fires after its response
    /// flushes. Together with [`Self::with_config_source`], this enables the
    /// pages.
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
        let listener =
            rusty_photon_tls::server::bind_dual_stack_tokio(self.config.server.socket_addr())
                .await?;
        let local_addr = listener.local_addr()?;
        let tls = self.config.server.tls.clone();
        let built = self.build_for(local_addr).await?;
        Ok(BoundServer {
            listener,
            local_addr,
            tls,
            built,
        })
    }

    /// Consume the builder and build what the config describes for a
    /// server listening at `local_addr`: the router, the device behind it,
    /// and the discovery responder advertising it.
    async fn build_for(
        self,
        local_addr: SocketAddr,
    ) -> Result<Built, Box<dyn std::error::Error + Send + Sync>> {
        let mut server = Server::new(CargoServerInfo!());
        server.listen_addr = self.config.server.socket_addr();

        // The setup pages: one handler behind the server page and the device
        // page, present when the builder was given a config source and a
        // reload signal (the normal path through `run_server_loop`).
        let pages = match (self.config_path.clone(), self.reload.clone()) {
            (Some(path), Some(reload)) => Some(Arc::new(
                SetupPages::<FileMonitorDriver>::new("filemonitor", self.config.clone(), path, ())
                    .with_reload(reload)
                    .with_https(self.config.server.tls.is_some()),
            )),
            _ => None,
        };
        let mut device = FileMonitorDevice::new(self.config.clone());
        if let Some(pages) = &pages {
            server.setup = Some(Arc::new(rusty_photon_driver::ServerPages(Arc::clone(
                pages,
            ))));
            device = device.with_setup_pages(Arc::clone(pages));
        }
        let device = Arc::new(device);
        let registered: Arc<dyn SafetyMonitor> = device.clone();
        server.devices.register(registered);

        info!(
            "Starting ASCOM Alpaca server on port {}",
            self.config.server.port
        );
        info!(
            "Device: {} ({})",
            self.config.device.name, self.config.device.unique_id
        );
        info!("Monitoring file: {:?}", self.config.file.path);

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

        // Opt-in Alpaca UDP discovery responder (config `discovery_port`);
        // bound here so a taken port fails startup, run while the server
        // waits for its next shutdown or reload.
        let discovery =
            rusty_photon_driver::discovery::bind(local_addr, self.config.server.discovery_port)
                .await?;

        // Console mode only: stdout is a dead handle under the Windows SCM,
        // and the only stdout consumer (bdd-infra's port parser) never runs
        // services with --service.
        if !rusty_photon_service_lifecycle::is_scm_service() {
            println!("Bound Alpaca server bound_addr={local_addr}");
        }

        Ok(Built {
            router,
            device,
            discovery,
        })
    }
}

/// What one configuration builds for a server: the router that answers
/// its requests, the device behind the router, and the discovery responder
/// advertising the server.
struct Built {
    router: axum::Router,
    device: Arc<FileMonitorDevice>,
    discovery: Option<ascom_alpaca::discovery::BoundDiscoveryServer>,
}

impl Built {
    /// Connect the device before its router answers, as a client's
    /// `Connected = true` would: the reload's reconnect of a device that was
    /// connected before it. It reads the watched file and starts polling,
    /// and moves nothing. A failure (the file cannot be read) leaves the
    /// device disconnected, as its clients then find it.
    async fn connect_device(&self) {
        match self.device.set_connected(true).await {
            Ok(()) => debug!("Reconnected the rebuilt device"),
            Err(e) => warn!(
                "The device was connected before the reload, but the rebuilt one could not connect, so it stays disconnected: {}",
                e.message
            ),
        }
    }
}

/// A fully bound filemonitor server ready to accept connections.
pub struct BoundServer {
    listener: TcpListener,
    local_addr: SocketAddr,
    tls: Option<TlsConfig>,
    built: Built,
}

impl BoundServer {
    pub const fn listen_addr(&self) -> SocketAddr {
        self.local_addr
    }

    /// Serve the bound listener (and the discovery responder, when bound)
    /// until `shutdown` resolves, then until every connection it accepted
    /// has finished, and disconnect the device.
    ///
    /// # Errors
    ///
    /// Returns an error if the TLS material cannot be loaded or accepting
    /// connections fails.
    pub async fn start(
        self,
        shutdown: impl Future<Output = ()> + Send + 'static,
    ) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        let Self {
            listener,
            local_addr,
            tls,
            built,
        } = self;
        let stop = rusty_photon_driver::discovery::serve_with(built.discovery, shutdown);
        let draining = accept(&listener, local_addr, tls, built.router, stop).await?;
        drop(listener);
        draining.finished().await;
        built.device.disconnect().await;
        Ok(())
    }
}

/// Accept on `listener`, over TLS when `tls` is set, answering every
/// connection with `router`, until `stop` resolves. Return as soon as the
/// accepting has stopped, with the connections it accepted still finishing.
async fn accept(
    listener: &TcpListener,
    local_addr: SocketAddr,
    tls: Option<TlsConfig>,
    router: axum::Router,
    stop: impl Future<Output = ()>,
) -> Result<Draining, Box<dyn std::error::Error + Send + Sync>> {
    let draining = if let Some(ref tls_config) = tls {
        let acceptor = rusty_photon_tls::server::build_tls_acceptor(tls_config)?;
        info!("filemonitor started on {} (TLS)", local_addr);
        rusty_photon_tls::server::serve_tls_on(listener, router, acceptor, stop).await?
    } else {
        info!("filemonitor started on {}", local_addr);
        rusty_photon_tls::server::serve_plain_on(listener, router, stop).await?
    };
    debug!("filemonitor stopped accepting");
    Ok(draining)
}

/// Wait until every request the replaced router took has been answered,
/// then disconnect `device`, the device behind it, which stops its polling.
async fn retire(device: Arc<FileMonitorDevice>, replaced: Replaced) {
    replaced.answered().await;
    device.disconnect().await;
    debug!("A replaced filemonitor device has answered its last request; it is disconnected");
}

/// Build a fresh `BoundServer` from a `Config` and run it until the
/// `shutdown` future resolves.
///
/// The drain is [`BoundServer::start`]'s: stop accepting, close the
/// listener, wait for every connection to finish, then disconnect the
/// device.
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

/// Why the server stopped accepting.
enum Restart {
    Shutdown,
    /// A reload changed the address or the TLS setting: the new
    /// configuration.
    Transport(Box<Config>),
}

/// What [`run_server_loop`] keeps across reloads.
struct Reloading<'a> {
    config_path: &'a Path,
    shutdown: CancellationToken,
    reload: ReloadSignal,
    /// The configuration the server answers with now.
    config: Config,
    /// The router every connection answers with.
    routes: Routes,
    /// The device behind the current router.
    device: Arc<FileMonitorDevice>,
    /// The current discovery responder, until the wait for the next
    /// shutdown or reload takes it.
    discovery: Option<ascom_alpaca::discovery::BoundDiscoveryServer>,
    /// The replaced devices, and the connections of an accepting a changed
    /// address or TLS setting stopped, still finishing.
    retiring: tokio::task::JoinSet<()>,
}

impl Reloading<'_> {
    fn builder(&self, config: Config) -> ServerBuilder {
        ServerBuilder::new(config)
            .with_config_source(self.config_path.to_path_buf())
            .with_reload_signal(self.reload.clone())
    }

    /// Answer each reload that keeps the address and the TLS setting by
    /// putting the rebuilt server in front of the connections, until
    /// shutdown or a reload that changes either. The discovery responder
    /// answers while it waits, and stops when the wait ends.
    async fn until_restart(
        &mut self,
        local_addr: SocketAddr,
    ) -> Result<Restart, Box<dyn std::error::Error + Send + Sync>> {
        loop {
            let shutdown = self.shutdown.clone();
            let reload = self.reload.clone();
            let reloaded =
                rusty_photon_driver::discovery::serve_with(self.discovery.take(), async {
                    tokio::select! {
                        () = shutdown.cancelled() => false,
                        () = reload.recv() => true,
                    }
                })
                .await;
            if !reloaded {
                return Ok(Restart::Shutdown);
            }
            info!("Reloading configuration");
            let config = load_config(self.config_path)?;
            if config.server.socket_addr() != self.config.server.socket_addr()
                || config.server.tls != self.config.server.tls
            {
                return Ok(Restart::Transport(Box::new(config)));
            }
            self.rebuild(config, local_addr).await?;
        }
    }

    /// Build the server `config` describes for the listener at
    /// `local_addr` and put it in front of every connection. Its device is
    /// connected first when the one it replaces was; the replaced device
    /// stays connected until the requests its router took are answered.
    async fn rebuild(
        &mut self,
        config: Config,
        local_addr: SocketAddr,
    ) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        let built = self.builder(config.clone()).build_for(local_addr).await?;
        if *self.device.connected.read().await {
            built.connect_device().await;
        }
        let replaced = self.routes.replace(built.router);
        let device = std::mem::replace(&mut self.device, built.device);
        _ = self.retiring.spawn(retire(device, replaced));
        while self.retiring.try_join_next().is_some() {}
        self.discovery = built.discovery;
        self.config = config;
        Ok(())
    }
}

/// Run filemonitor's server in a config-reload loop until `shutdown`
/// fires.
///
/// A reload (the `reload` a setup-page save fires, `SIGHUP`, or `ParamChange`)
/// keeps the server's clients. While the configured address and TLS setting
/// are unchanged the server keeps accepting and closes no connection: every
/// connection answers each request with the router current when it arrives,
/// and the reload puts the rebuilt server's router there. When the old
/// device was connected, the rebuilt one is connected first. The old device
/// stays connected until the requests the old router took are answered. A
/// changed address or TLS setting stops the accepting, and its connections
/// close once their requests are answered; a changed address closes the
/// old listener before the new one is bound, since the same port on a
/// narrower address does not bind while the old socket listens. Shutdown
/// closes the listener and waits for every connection to finish.
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
    let config = load_config(config_path)?;
    info!("Starting filemonitor server on port {}", config.server.port);
    let listener =
        rusty_photon_tls::server::bind_dual_stack_tokio(config.server.socket_addr()).await?;
    serve_reloading(config_path, config, listener, shutdown, reload).await
}

/// [`run_server_loop`] from its first config, on a listener already bound
/// for that config's address.
async fn serve_reloading(
    config_path: &Path,
    config: Config,
    mut listener: TcpListener,
    shutdown: CancellationToken,
    reload: ReloadSignal,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let mut local_addr = listener.local_addr()?;
    let built = ServerBuilder::new(config.clone())
        .with_config_source(config_path.to_path_buf())
        .with_reload_signal(reload.clone())
        .build_for(local_addr)
        .await?;
    let mut reloading = Reloading {
        config_path,
        shutdown,
        reload,
        config,
        routes: Routes::new(built.router),
        device: built.device,
        discovery: built.discovery,
        retiring: tokio::task::JoinSet::new(),
    };
    loop {
        let tls = reloading.config.server.tls.clone();
        let service = reloading.routes.service();
        let mut restart = Ok(Restart::Shutdown);
        let draining = accept(&listener, local_addr, tls, service, async {
            restart = reloading.until_restart(local_addr).await;
        })
        .await?;

        let Restart::Transport(config) = restart? else {
            drop(listener);
            draining.finished().await;
            reloading.device.disconnect().await;
            while reloading.retiring.join_next().await.is_some() {}
            return Ok(());
        };
        _ = reloading.retiring.spawn(draining.finished());
        let addr = config.server.socket_addr();
        if addr == reloading.config.server.socket_addr() {
            debug!("Keeping the listener on {}", addr);
        } else {
            info!("Starting filemonitor server on port {}", config.server.port);
            drop(listener);
            listener = rusty_photon_tls::server::bind_dual_stack_tokio(addr).await?;
            local_addr = listener.local_addr()?;
        }
        reloading.rebuild(*config, local_addr).await?;
    }
}

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
mod device_setup_page_tests {
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

    /// Build a device wired with the setup pages over a temp file pre-seeded
    /// with `effective`. Returns the reload handle (clone) so tests can assert
    /// the fire-after-response reload, and the `TempDir`/path.
    fn device_with_setup_pages(
        effective: Config,
    ) -> (FileMonitorDevice, ReloadSignal, tempfile::TempDir, PathBuf) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("filemonitor.json");
        std::fs::write(&path, serde_json::to_string(&effective).unwrap()).unwrap();
        let reload = ReloadSignal::new();
        let pages = SetupPages::<FileMonitorDriver>::new(
            "filemonitor",
            effective.clone(),
            path.clone(),
            (),
        )
        .with_reload(reload.clone());
        let device = FileMonitorDevice::new(effective).with_setup_pages(Arc::new(pages));
        (device, reload, dir, path)
    }

    fn get_request() -> SetupRequest {
        SetupRequest {
            method: axum::http::Method::GET,
            headers: axum::http::HeaderMap::new(),
            query: Vec::new(),
            form: Vec::new(),
        }
    }

    /// A `POST` of the device page's form, from the page itself.
    fn post_request(form: &[(&str, &str)]) -> SetupRequest {
        let mut headers = axum::http::HeaderMap::new();
        _ = headers.insert(
            "sec-fetch-site",
            axum::http::HeaderValue::from_static("same-origin"),
        );
        SetupRequest {
            method: axum::http::Method::POST,
            headers,
            query: Vec::new(),
            form: form
                .iter()
                .map(|(k, v)| ((*k).to_string(), (*v).to_string()))
                .collect(),
        }
    }

    #[tokio::test]
    async fn the_config_actions_are_gone() {
        let (device, _reload, _dir, _path) = device_with_setup_pages(test_config());
        assert_eq!(
            device.supported_actions().await.unwrap(),
            Vec::<String>::new()
        );
        let err = device
            .action("config.get".to_string(), String::new())
            .await
            .unwrap_err();
        assert_eq!(err.code, ASCOMErrorCode::ACTION_NOT_IMPLEMENTED);
    }

    #[tokio::test]
    async fn the_device_page_renders_the_device_section_while_disconnected() {
        let (device, _reload, _dir, _path) = device_with_setup_pages(test_config());
        assert!(!device.connected().await.unwrap());
        let response = device.setup_request(get_request()).await;
        assert_eq!(response.status(), axum::http::StatusCode::OK);
        let body = response.body();
        assert!(
            body.contains(r#"name="device.unique_id" value="filemonitor-test-id""#),
            "{body}"
        );
        assert!(!body.contains(r#"name="file.path""#), "{body}");
    }

    #[tokio::test]
    #[cfg_attr(miri, ignore)] // the save fsyncs the config's parent directory, which Miri cannot open
    async fn a_device_page_save_persists_the_device_section_alone_and_fires_the_reload() {
        let (device, reload, _dir, path) = device_with_setup_pages(test_config());
        let response = device
            .setup_request(post_request(&[
                ("device.name", "Roof monitor"),
                ("device.unique_id", "filemonitor-test-id"),
                ("device.description", "Test"),
            ]))
            .await;
        assert_eq!(
            response.status(),
            axum::http::StatusCode::OK,
            "{}",
            response.body()
        );
        let persisted: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(
            persisted
                .pointer("/device/name")
                .and_then(serde_json::Value::as_str),
            Some("Roof monitor")
        );
        // humantime spells the minute the test config holds as "1m".
        assert_eq!(
            persisted
                .pointer("/file/polling_interval")
                .and_then(serde_json::Value::as_str),
            Some("1m")
        );
        tokio::time::timeout(Duration::from_secs(2), reload.recv())
            .await
            .expect("a changed save fires the reload");
    }

    #[tokio::test]
    async fn a_rejected_device_page_save_names_the_field_and_leaves_the_file() {
        let (device, _reload, _dir, path) = device_with_setup_pages(test_config());
        let before = std::fs::read_to_string(&path).unwrap();
        let response = device
            .setup_request(post_request(&[
                ("device.name", "Roof monitor"),
                ("device.unique_id", ""),
                ("device.description", "Test"),
            ]))
            .await;
        assert_eq!(
            response.status(),
            axum::http::StatusCode::UNPROCESSABLE_ENTITY,
            "{}",
            response.body()
        );
        assert!(
            response
                .body()
                .contains("must not be empty (it is the device"),
            "{}",
            response.body()
        );
        assert_eq!(std::fs::read_to_string(&path).unwrap(), before);
    }

    #[tokio::test]
    async fn a_device_without_pages_has_no_setup_page() {
        let device = FileMonitorDevice::new(test_config());
        let response = device.setup_request(get_request()).await;
        assert_eq!(response.status(), axum::http::StatusCode::NOT_FOUND);
    }
}

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
mod reload_tests {
    use super::*;
    use std::time::Instant;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpStream;

    /// How long a test waits for the loop to answer, on the slowest runner
    /// the suites meet: a bound on an answer that never comes, not an
    /// expected duration.
    const BUDGET: Duration = Duration::from_secs(30);

    /// A config serving on loopback, watching a file holding "CLOSED",
    /// with the discovery responder on `discovery_port` when given.
    fn loopback_config(dir: &Path, discovery_port: Option<u16>) -> Config {
        let watched = dir.join("RoofStatusFile.txt");
        std::fs::write(&watched, "CLOSED").unwrap();
        let mut server = AlpacaServerConfig::new(0);
        server.bind_address = std::net::Ipv4Addr::LOCALHOST.into();
        server.discovery_port = discovery_port;
        Config {
            server,
            file: FileConfig {
                path: watched,
                polling_interval: Duration::from_mins(1),
            },
            ..Config::default()
        }
    }

    /// The reload loop, running on a listener the test bound.
    struct Running {
        port: u16,
        config_path: PathBuf,
        shutdown: CancellationToken,
        reload: ReloadSignal,
        task: tokio::task::JoinHandle<Result<(), Box<dyn std::error::Error + Send + Sync>>>,
    }

    impl Running {
        async fn start(dir: &Path, config: &Config) -> Self {
            let config_path = dir.join("filemonitor.json");
            std::fs::write(&config_path, serde_json::to_string(config).unwrap()).unwrap();
            let listener =
                rusty_photon_tls::server::bind_dual_stack_tokio(config.server.socket_addr())
                    .await
                    .unwrap();
            let port = listener.local_addr().unwrap().port();
            let shutdown = CancellationToken::new();
            let reload = ReloadSignal::new();
            let task = tokio::spawn({
                let config_path = config_path.clone();
                let config = config.clone();
                let shutdown = shutdown.clone();
                let reload = reload.clone();
                async move { serve_reloading(&config_path, config, listener, shutdown, reload).await }
            });
            Self {
                port,
                config_path,
                shutdown,
                reload,
                task,
            }
        }

        /// Save `config` and fire the reload.
        fn reload_with(&self, config: &Config) {
            std::fs::write(&self.config_path, serde_json::to_string(config).unwrap()).unwrap();
            self.reload.notify();
        }

        async fn stop(self) {
            self.shutdown.cancel();
            self.task.await.unwrap().unwrap();
        }
    }

    /// Send a plain HTTP/1.1 `GET` of `path` over `stream` and return the
    /// response head, reading the body its `Content-Length` announces.
    async fn get(stream: &mut TcpStream, port: u16, path: &str) -> String {
        let request = format!("GET {path} HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\n\r\n");
        stream.write_all(request.as_bytes()).await.unwrap();
        let mut head = Vec::new();
        while !head.ends_with(b"\r\n\r\n") {
            let mut byte = [0u8; 1];
            assert_eq!(stream.read(&mut byte).await.unwrap(), 1, "{head:?}");
            head.extend_from_slice(&byte);
        }
        let head = String::from_utf8(head).unwrap();
        let length = head
            .lines()
            .find_map(|line| {
                let (name, value) = line.split_once(':')?;
                name.eq_ignore_ascii_case("content-length")
                    .then(|| value.trim().parse::<usize>().unwrap())
            })
            .unwrap_or(0);
        let mut body = vec![0u8; length];
        stream.read_exact(&mut body).await.unwrap();
        head
    }

    /// The device's description, as an Alpaca client reads it.
    async fn description(port: u16) -> Option<String> {
        let url = format!(
            "http://127.0.0.1:{port}/api/v1/safetymonitor/0/description?ClientID=1&ClientTransactionID=1"
        );
        let body: serde_json::Value = reqwest::get(url).await.ok()?.json().await.ok()?;
        body["Value"].as_str().map(str::to_string)
    }

    #[tokio::test]
    #[cfg_attr(miri, ignore)] // binds sockets, which Miri cannot
    async fn a_replaced_device_is_disconnected_once_its_router_has_answered() {
        let dir = tempfile::tempdir().unwrap();
        let built = ServerBuilder::new(loopback_config(dir.path(), None))
            .build_for(SocketAddr::from(([127, 0, 0, 1], 0)))
            .await
            .unwrap();
        built.connect_device().await;
        let device = Arc::clone(&built.device);

        // A router holding a request until the test releases it.
        let (release, released) = tokio::sync::oneshot::channel::<()>();
        let released = Arc::new(Mutex::new(Some(released)));
        let (arrived, has_arrived) = tokio::sync::oneshot::channel::<()>();
        let arrived = Arc::new(Mutex::new(Some(arrived)));
        let held = axum::Router::new().route(
            "/",
            axum::routing::get(move || async move {
                if let Some(arrived) = arrived.lock().await.take() {
                    arrived.send(()).unwrap();
                }
                if let Some(released) = released.lock().await.take() {
                    released.await.unwrap();
                }
            }),
        );
        let routes = Routes::new(held);
        let in_flight = tokio::spawn({
            let service = routes.service();
            async move {
                tower::ServiceExt::oneshot(
                    service,
                    axum::extract::Request::get("/")
                        .body(axum::body::Body::empty())
                        .unwrap(),
                )
                .await
                .unwrap()
            }
        });
        has_arrived.await.unwrap();

        let retiring = retire(Arc::clone(&device), routes.replace(axum::Router::new()));
        tokio::pin!(retiring);
        tokio::select! {
            biased;
            () = &mut retiring => panic!("the device retired while its router was answering"),
            () = std::future::ready(()) => {}
        }
        assert!(
            device.connected().await.unwrap(),
            "the replaced device should stay connected until its router has answered"
        );

        release.send(()).unwrap();
        in_flight.await.unwrap();
        retiring.await;
        assert!(!device.connected().await.unwrap());
        assert!(
            device.polling_handle.lock().await.is_none(),
            "a retired device should stop polling its file"
        );
    }

    #[tokio::test]
    #[cfg_attr(miri, ignore)] // binds sockets, which Miri cannot
    async fn a_reload_rebinds_the_discovery_port_the_replaced_server_held() {
        let dir = tempfile::tempdir().unwrap();
        let discovery_port = std::net::UdpSocket::bind("127.0.0.1:0")
            .unwrap()
            .local_addr()
            .unwrap()
            .port();
        let mut config = loopback_config(dir.path(), Some(discovery_port));
        let running = Running::start(dir.path(), &config).await;

        config.device.description = "rebuilt".to_string();
        running.reload_with(&config);
        let started = Instant::now();
        while description(running.port).await.as_deref() != Some("rebuilt") {
            assert!(
                started.elapsed() < BUDGET,
                "the rebuilt server did not answer within {:?}",
                started.elapsed()
            );
            tokio::time::sleep(Duration::from_millis(50)).await;
        }

        // A socket without address reuse binds only a port no other socket
        // holds.
        assert!(
            std::net::UdpSocket::bind(("127.0.0.1", discovery_port)).is_err(),
            "the rebuilt server should hold the discovery port"
        );
        running.stop().await;
    }

    #[tokio::test]
    #[cfg_attr(miri, ignore)] // binds sockets, which Miri cannot
    async fn a_changed_tls_setting_closes_the_old_connections_and_accepts_over_tls() {
        let dir = tempfile::tempdir().unwrap();
        let mut config = loopback_config(dir.path(), None);
        let running = Running::start(dir.path(), &config).await;
        let mut kept = TcpStream::connect(("127.0.0.1", running.port))
            .await
            .unwrap();
        let head = get(&mut kept, running.port, "/management/apiversions").await;
        assert!(head.starts_with("HTTP/1.1 200"), "{head}");

        rusty_photon_tls::test_cert::generate_ca(dir.path()).unwrap();
        rusty_photon_tls::test_cert::generate_service_cert(
            &std::fs::read_to_string(dir.path().join("ca.pem")).unwrap(),
            &std::fs::read_to_string(dir.path().join("ca-key.pem")).unwrap(),
            "filemonitor",
            dir.path(),
        )
        .unwrap();
        config.server.tls = Some(TlsConfig {
            cert: dir.path().join("filemonitor.pem").display().to_string(),
            key: dir.path().join("filemonitor-key.pem").display().to_string(),
        });
        running.reload_with(&config);

        let mut byte = [0u8; 1];
        let closed = tokio::time::timeout(BUDGET, kept.read(&mut byte))
            .await
            .unwrap_or_else(|_| panic!("the old connection was still open after {BUDGET:?}"));
        assert!(
            matches!(closed, Ok(0) | Err(_)),
            "the old connection should be closed, but read {closed:?}"
        );
        let mut fresh = TcpStream::connect(("127.0.0.1", running.port))
            .await
            .unwrap();
        let head = get(&mut fresh, running.port, "/management/apiversions").await;
        assert!(
            head.starts_with("HTTP/1.1 308"),
            "plain HTTP on the same port should now be redirected to TLS: {head}"
        );
        running.stop().await;
    }

    #[tokio::test]
    #[cfg_attr(miri, ignore)] // binds sockets, which Miri cannot
    async fn a_device_whose_file_is_missing_stays_disconnected_when_the_reload_connects_it() {
        let dir = tempfile::tempdir().unwrap();
        let mut config = loopback_config(dir.path(), None);
        config.file.path = dir.path().join("missing.txt");
        let built = ServerBuilder::new(config)
            .build_for(SocketAddr::from(([127, 0, 0, 1], 0)))
            .await
            .unwrap();

        built.connect_device().await;

        assert!(!built.device.connected().await.unwrap());
    }
}

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
mod regex_rule_tests {
    use super::*;
    use std::path::PathBuf;

    fn config_with_regex_rule(pattern: &str, safe: bool, case_sensitive: bool) -> Config {
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
                rules: vec![ParsingRule {
                    rule_type: RuleType::Regex,
                    pattern: pattern.to_string(),
                    safe,
                }],
                case_sensitive,
            },
            server: AlpacaServerConfig::new(8080),
        }
    }

    #[test]
    fn a_case_insensitive_regex_rule_is_compiled_under_i() {
        let insensitive = config_with_regex_rule("^open$", true, false);
        let compiled = insensitive
            .parsing
            .compile_regex(&insensitive.parsing.rules[0])
            .unwrap();
        assert!(compiled.is_match("OPEN"));

        let sensitive = config_with_regex_rule("^open$", true, true);
        let compiled = sensitive
            .parsing
            .compile_regex(&sensitive.parsing.rules[0])
            .unwrap();
        assert!(!compiled.is_match("OPEN"));
    }

    #[test]
    fn an_uncompilable_regex_rule_never_matches_so_the_file_reads_unsafe() {
        // The content is the pattern's own text: a fallback to a plain
        // substring match would read it safe. The regex never matches, and a
        // file no rule matches is unsafe.
        let device = FileMonitorDevice::new(config_with_regex_rule("(unclosed", true, false));
        assert!(!device.evaluate_safety("(unclosed"));
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
    fn default_scaffold_round_trips_through_load() {
        // The startup bootstrap writes `Config::default()` to the platform
        // path on first start, and only once it loads as `Config`.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("filemonitor.json");
        let scaffold = serde_json::to_string_pretty(&Config::default()).unwrap();
        std::fs::write(&path, scaffold).unwrap();

        let c = load_config(&path).unwrap();

        assert_eq!(c.server.port, 11111);
    }

    /// Writes `config` to a file in `dir` and loads it back.
    fn load_written(
        dir: &tempfile::TempDir,
        config: &Config,
    ) -> Result<Config, Box<dyn std::error::Error + Send + Sync>> {
        let path = dir.path().join("filemonitor.json");
        std::fs::write(&path, serde_json::to_string_pretty(config).unwrap()).unwrap();
        load_config(&path)
    }

    #[test]
    fn the_load_refuses_a_zero_polling_interval() {
        let dir = tempfile::tempdir().unwrap();
        let mut config = Config::default();
        config.file.polling_interval = Duration::ZERO;

        let err = load_written(&dir, &config).unwrap_err().to_string();

        assert_eq!(err, "file.polling_interval must be greater than 0");
    }

    #[test]
    fn the_load_refuses_a_rule_whose_regex_does_not_compile() {
        let dir = tempfile::tempdir().unwrap();
        let mut config = Config::default();
        config.parsing.rules[2].pattern = "(unclosed".to_string();

        let err = load_written(&dir, &config).unwrap_err().to_string();

        assert!(
            err.starts_with("parsing.rules.2.pattern invalid regex:"),
            "{err}"
        );
    }

    #[test]
    fn the_load_refuses_an_empty_file_path_and_unique_id_naming_the_first() {
        let dir = tempfile::tempdir().unwrap();
        let mut config = Config::default();
        config.device.unique_id = String::new();
        config.file.path = PathBuf::new();

        let err = load_written(&dir, &config).unwrap_err().to_string();

        assert_eq!(
            err,
            "device.unique_id must not be empty (it is the device's stable ASCOM UniqueID)"
        );
    }

    #[test]
    fn the_scaffold_passes_the_check_the_bootstrap_holds_it_to() {
        Config::default().check().unwrap();
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

#[cfg(test)]
mod persisted_config_shape {
    use rusty_photon_server_config::unset::explicit_nulls;

    use super::Config;

    /// An unset optional field is spelled by its key's absence, never by an
    /// explicit `null` — see [`rusty_photon_server_config::unset`] for why.
    /// A field that grows without `skip_serializing_if` trips here rather
    /// than filling operators' config files with nulls.
    #[test]
    fn the_default_config_persists_no_explicit_nulls() {
        let persisted = serde_json::to_value(Config::default()).unwrap();
        assert_eq!(
            explicit_nulls(&persisted),
            Vec::<String>::new(),
            "unset optional fields must be omitted, not written as null: {persisted}"
        );
    }
}
