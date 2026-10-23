//! World struct for pa-falcon-rotator BDD tests
//!
//! Runs the ASCOM Alpaca server in-process on an ephemeral port so the
//! test holds the same `Arc<MockFalconTransportFactory>` the `FalconManager`
//! drives. That gives every scenario direct access to mock device state
//! (set the reported mechanical position, voltage, `motor_reverse`,
//! `limit_detect` flag) and to the wire-level `command_log` (assert which
//! commands were sent and in what order) — both of which the feature
//! files exercise.
//!
//! Devices are driven via Alpaca HTTP through the in-process client, so
//! the real serialisation / dispatch path is still exercised. The
//! `start_service` harness sets `config.server.auth = None`; the TLS +
//! auth credential gate is covered by the shared smoke scenario in
//! `auth.feature` via the `TlsAuthSmokeWorld` impl below.
//!
//! The setup-page scenarios (`setup_pages.feature`) are the exception:
//! a save reloads the process that served it, which only the binary's
//! `main` can do, so they spawn the mock-feature binary through
//! [`ServiceHandle`] (`start_process`) like the other serial drivers'
//! suites.

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use ascom_alpaca::api::{Rotator, Switch, TypedDevice};
use ascom_alpaca::Client as AlpacaClient;
use bdd_infra::setup_pages::{self, SetupPagesState, SetupPagesWorld};
use bdd_infra::tls_auth::{TlsAuthSmokeWorld, TlsAuthState};
use bdd_infra::ServiceHandle;
use cucumber::World;
use pa_falcon_rotator::{Config, MockFalconTransportFactory, ServerBuilder};
use rusty_photon_shared_transport::TransportFactory;
use tempfile::TempDir;
use tokio::task::JoinHandle;

// Several `*_result` fields below are still wired up by Phase 3e step bodies
// (status switch) — they live here so the struct's shape is stable across the
// remaining sub-phases.
#[allow(dead_code)]
#[derive(Debug, Default, World)]
pub struct FalconRotatorWorld {
    /// Joined when the scenario ends to tear the in-process server down.
    pub server_handle: Option<JoinHandle<()>>,
    /// Bound address of the in-process server (ephemeral port).
    pub server_addr: Option<SocketAddr>,
    /// Mock factory shared with the `FalconManager` — drives device state
    /// and records the wire-level command log.
    pub mock: Option<Arc<MockFalconTransportFactory>>,
    /// Alpaca HTTP client for the Rotator device.
    pub rotator: Option<Arc<dyn Rotator>>,
    /// Alpaca HTTP client for the Status Switch device.
    pub status_switch: Option<Arc<dyn Switch>>,
    /// Config used to build the server (mutated by Given/setup steps).
    pub config: Option<Config>,

    /// Captured property reads.
    pub position_result: Option<f64>,
    pub mechanical_position_result: Option<f64>,
    pub target_position_result: Option<f64>,
    pub is_moving_result: Option<bool>,
    pub reverse_result: Option<bool>,
    pub step_size_result: Option<f64>,
    pub can_reverse_result: Option<bool>,
    pub name_result: Option<String>,
    pub unique_id_result: Option<String>,
    pub interface_version_result: Option<i32>,

    /// Captured switch reads.
    pub switch_value_result: Option<f64>,
    pub switch_bool_result: Option<bool>,
    pub max_switch_result: Option<usize>,
    /// Captured `GetSwitchName` / `GetSwitchDescription` reads.
    pub switch_string_result: Option<String>,

    /// Last failure surfaced over Alpaca, if any.
    pub last_error_code: Option<u16>,

    /// Result of the last `supported_actions` query.
    pub last_supported_actions: Option<Vec<String>>,

    /// The service as a spawned process, for the setup-page scenarios: a
    /// save reloads the process, which the in-process server cannot do.
    pub process: Option<ServiceHandle>,
    /// The spawned process's config directory, alive for the scenario.
    pub process_dir: Option<TempDir>,
    /// State for the shared setup-page steps (`setup_pages.feature`).
    pub setup_pages: SetupPagesState,

    /// State for the shared TLS + auth smoke steps (`auth.feature`).
    pub tls_auth: TlsAuthState,

    /// Doctor-subcommand smoke state (staged config file + run output)
    pub doctor_smoke: bdd_infra::doctor_smoke::DoctorSmokeState,
}

impl bdd_infra::doctor_smoke::DoctorSmokeWorld for FalconRotatorWorld {
    fn doctor_smoke(&mut self) -> &mut bdd_infra::doctor_smoke::DoctorSmokeState {
        &mut self.doctor_smoke
    }

    fn valid_config(&self) -> serde_json::Value {
        let mut config = self.base_test_config();
        config["server"] = serde_json::json!({ "port": 0 });
        config
    }
}

impl TlsAuthSmokeWorld for FalconRotatorWorld {
    fn tls_auth(&mut self) -> &mut TlsAuthState {
        &mut self.tls_auth
    }

    fn base_test_config(&self) -> serde_json::Value {
        // The library defaults with the mock serial port; the shared
        // configure step replaces `server` with the fixture's block.
        let mut config = serde_json::to_value(Config::default()).unwrap();
        config["serial"]["port"] = serde_json::json!("/dev/mock");
        config
    }

    async fn start_with_tls_auth(&mut self, config: serde_json::Value) {
        // Deserialize through the typed `Config` so the smoke exercises the
        // real config-load path: the shared `server` block (port 0 + `tls` +
        // `auth`) maps onto `AlpacaServerConfig`.
        let config: Config = serde_json::from_value(config).unwrap();

        let mock = Arc::new(MockFalconTransportFactory::default());
        let factory: Arc<dyn TransportFactory> = Arc::clone(&mock) as _;
        let bound = ServerBuilder::new()
            .with_config(config)
            .with_factory(factory)
            .build()
            .await
            .expect("build in-process Alpaca server with TLS + auth");
        let local_addr = bound.listen_addr();

        let server_handle = tokio::spawn(async move {
            let _ = bound.start(std::future::pending::<()>()).await;
        });
        self.mock = Some(mock);
        // Torn down by `World::shutdown` from the cucumber `after` hook.
        self.server_handle = Some(server_handle);
        self.server_addr = Some(SocketAddr::from(([127, 0, 0, 1], local_addr.port())));
        self.tls_auth.port = Some(bdd_infra::BoundPort::fixed(local_addr.port()));
    }
}

impl SetupPagesWorld for FalconRotatorWorld {
    fn setup_pages(&mut self) -> &mut SetupPagesState {
        &mut self.setup_pages
    }
}

impl FalconRotatorWorld {
    pub fn rotator(&self) -> &Arc<dyn Rotator> {
        self.rotator.as_ref().expect("rotator not acquired")
    }

    pub fn status_switch(&self) -> &Arc<dyn Switch> {
        self.status_switch
            .as_ref()
            .expect("status switch not acquired")
    }

    pub const fn mock(&self) -> &Arc<MockFalconTransportFactory> {
        self.mock.as_ref().expect("mock not initialised")
    }

    /// Build a config, start the in-process Alpaca server with the mock
    /// serial factory, and acquire client proxies for both registered
    /// devices.
    pub async fn start_service(&mut self) {
        let mut config = self.config.clone().unwrap_or_default();
        // The in-process world bypasses main.rs's first-run identity
        // materialization, so seed stable UniqueIDs here (mirroring what the
        // binary persists on first run) for deterministic metadata assertions.
        if config.rotator.unique_id.is_empty() {
            config.rotator.unique_id = "pa-falcon-rotator-001".to_string();
        }
        if config.switch.unique_id.is_empty() {
            config.switch.unique_id = "pa-falcon-rotator-status-001".to_string();
        }
        // Bind on an ephemeral port so concurrent BDD scenarios don't fight
        // over a fixed port number.
        config.server.port = 0;
        // No UDP discovery service — the test resolves the device list via
        // the bound HTTP port directly.
        config.server.discovery_port = None;
        // No TLS / auth in BDD scenarios.
        config.server.tls = None;
        config.server.auth = None;

        let mock = Arc::new(MockFalconTransportFactory::default());
        let factory: Arc<dyn TransportFactory> = Arc::clone(&mock) as _;

        // No config source and no reload signal: the in-process server has
        // no setup pages. The setup-page scenarios spawn the binary instead
        // (`start_process`), whose `main` reloads it on a save.
        let bound = ServerBuilder::new()
            .with_config(config)
            .with_factory(factory)
            .build()
            .await
            .expect("build in-process Alpaca server");
        let local_addr = bound.listen_addr();

        let server_handle = tokio::spawn(async move {
            // BoundServer::start returns `Result<(), Box<dyn Error>>`; the
            // error type is `!Send` so we collapse it to `()` here before
            // letting the spawn machinery see the output. The shutdown
            // future never resolves; the test tears the task down by
            // aborting the JoinHandle in `World::shutdown`.
            if bound.start(std::future::pending::<()>()).await.is_err() {
                // tear-down on shutdown is normal; nothing to log.
            }
        });

        // Park the spawned task on `self` immediately so a panic inside
        // `acquire_devices` (it currently `panic!`s after a 5s timeout) does
        // not drop the `JoinHandle` and detach the in-process server. The
        // cucumber `after` hook calls `World::shutdown`, which aborts the
        // handle — but only if it's visible on the world.
        self.mock = Some(mock);
        self.server_handle = Some(server_handle);
        let http_addr = SocketAddr::from(([127, 0, 0, 1], local_addr.port()));
        self.server_addr = Some(http_addr);

        let (rotator, status_switch) = acquire_devices(http_addr).await;
        self.rotator = Some(rotator);
        self.status_switch = Some(status_switch);
    }

    /// The config the spawned process starts on: the library defaults with
    /// the mock serial port, the unique ids `start_service` seeds, and the
    /// `server` block every scenario shares — port 0 for OS-assigned, no
    /// discovery, no TLS, no auth.
    fn process_config(&self) -> serde_json::Value {
        let mut config = self.config.clone().unwrap_or_default();
        config.serial.port = "/dev/mock".to_string();
        if config.rotator.unique_id.is_empty() {
            config.rotator.unique_id = "pa-falcon-rotator-001".to_string();
        }
        if config.switch.unique_id.is_empty() {
            config.switch.unique_id = "pa-falcon-rotator-status-001".to_string();
        }
        let mut config = serde_json::to_value(config).expect("serialize config");
        config["server"] = serde_json::json!({ "port": 0, "discovery_port": null });
        config
    }

    /// Write the process's config file and return its path.
    fn write_process_config(&mut self) -> String {
        let config = self.process_config();
        let dir = self
            .process_dir
            .get_or_insert_with(|| TempDir::new().expect("failed to create temp dir"));
        let path = dir.path().join("config.json");
        std::fs::write(&path, config.to_string()).expect("failed to write config");
        path.to_str().unwrap().to_string()
    }

    /// Spawn the mock-feature binary on its config file and acquire clients
    /// for both devices.
    pub async fn start_process(&mut self) {
        let config_path = self.write_process_config();
        let handle = ServiceHandle::start(env!("CARGO_PKG_NAME"), &config_path).await;
        self.adopt_process(handle, config_path).await;
    }

    /// Spawn the binary with `serial.port` pinned by a `--port` override,
    /// so the server page shows the field pinned and never persists it.
    pub async fn start_process_with_serial_override(&mut self, serial_port: &str) {
        let config_path = self.write_process_config();
        let handle = ServiceHandle::start_with_args(
            env!("CARGO_PKG_NAME"),
            &["--config", &config_path, "--port", serial_port],
        )
        .await;
        self.adopt_process(handle, config_path).await;
    }

    /// Spawn the binary through the TLS + auth fixture, so the setup pages
    /// sit behind the credential.
    pub async fn start_process_with_tls_and_auth(&mut self) {
        let pki = bdd_infra::tls_auth::shared_pki(env!("CARGO_PKG_NAME")).await;
        self.tls_auth.pki = Some(pki);
        let mut config = self.process_config();
        config["server"] = self.tls_auth.pki().server_block(0);
        let handle = bdd_infra::tls_auth::spawn_service_handle(
            &mut self.tls_auth,
            env!("CARGO_PKG_NAME"),
            &config,
        )
        .await;
        let config_path = bdd_infra::tls_auth::staged_config_path(&self.tls_auth)
            .expect("the TLS fixture staged a config file");
        setup_pages::adopt_tls_auth(&mut self.setup_pages, &self.tls_auth, config_path);
        self.process = Some(handle);
    }

    /// Record a spawned plain-HTTP process: acquire both device clients and
    /// hand the setup-page steps its address and config file.
    async fn adopt_process(&mut self, handle: ServiceHandle, config_path: String) {
        let http_addr = SocketAddr::from(([127, 0, 0, 1], handle.port()));
        let (rotator, status_switch) = acquire_devices(http_addr).await;
        setup_pages::adopt_plain(
            &mut self.setup_pages,
            handle.bound_port(),
            config_path.into(),
        );
        self.rotator = Some(rotator);
        self.status_switch = Some(status_switch);
        self.server_addr = Some(http_addr);
        self.process = Some(handle);
    }

    /// Abort the in-process server task. Called from cucumber's `after` hook
    /// once per scenario so server resources are released before the next
    /// scenario binds its own ephemeral port.
    pub async fn shutdown(&mut self) {
        if let Some(handle) = self.server_handle.take() {
            handle.abort();
            // Drain the join result so the runtime sees the task as finished;
            // we don't care if it returned the JoinError(cancelled).
            let _ = handle.await;
        }
        self.rotator = None;
        self.status_switch = None;
        self.mock = None;
        self.server_addr = None;
        // A spawned process stops on drop.
        self.process = None;
    }
}

async fn acquire_devices(addr: SocketAddr) -> (Arc<dyn Rotator>, Arc<dyn Switch>) {
    let client = AlpacaClient::new_from_addr(addr);
    for _ in 0..200 {
        tokio::time::sleep(Duration::from_millis(25)).await;
        if let Ok(devices) = client.get_devices().await {
            let mut rotator = None;
            let mut status_switch = None;
            for device in devices {
                #[allow(unreachable_patterns)]
                match device {
                    TypedDevice::Rotator(r) => rotator = Some(r),
                    TypedDevice::Switch(s) => status_switch = Some(s),
                    _ => {}
                }
            }
            if let (Some(r), Some(s)) = (rotator, status_switch) {
                return (r, s);
            }
        }
    }
    panic!("pa-falcon-rotator in-process server did not register both devices in 5 seconds");
}
