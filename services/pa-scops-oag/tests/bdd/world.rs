//! World struct for pa-scops-oag BDD tests
//!
//! Uses binary spawning via `ServiceHandle` and HTTP interaction via `AlpacaClient`.

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use ascom_alpaca::api::{Focuser, TypedDevice};
use ascom_alpaca::Client as AlpacaClient;
use bdd_infra::setup_pages::{self, SetupPagesState, SetupPagesWorld};
use bdd_infra::tls_auth::TlsAuthState;
use bdd_infra::ServiceHandle;
use cucumber::World;
use pa_scops_oag::Config;
use tempfile::TempDir;

#[derive(Debug, Default, World)]
pub struct ScopsWorld {
    pub focuser_handle: Option<ServiceHandle>,
    pub focuser: Option<Arc<dyn Focuser>>,
    pub config: Option<Config>,
    pub temp_dir: Option<TempDir>,
    pub last_error: Option<String>,
    pub last_error_code: Option<u16>,
    pub position_result: Option<i32>,
    pub temperature_result: Option<f64>,
    pub is_moving_result: Option<bool>,

    /// Throwaway PKI + per-run credentials for the TLS/auth scenarios
    pub pki: Option<std::sync::Arc<bdd_infra::tls_auth::PkiFixture>>,

    /// Result of the last `supported_actions` query.
    pub last_supported_actions: Option<Vec<String>>,

    /// State for the shared setup-page steps (`setup_pages.feature`).
    pub setup_pages: SetupPagesState,
    /// The TLS + auth fixture the setup-page credential scenario starts
    /// the service through.
    pub tls_auth: TlsAuthState,

    /// Doctor-subcommand smoke state (staged config file + run output)
    pub doctor_smoke: bdd_infra::doctor_smoke::DoctorSmokeState,
}

impl bdd_infra::doctor_smoke::DoctorSmokeWorld for ScopsWorld {
    fn doctor_smoke(&mut self) -> &mut bdd_infra::doctor_smoke::DoctorSmokeState {
        &mut self.doctor_smoke
    }

    fn valid_config(&self) -> serde_json::Value {
        // The library defaults with the mock serial port and a plain
        // `server` block — the same shape the suite's start path parses.
        let mut config = serde_json::to_value(Config::default()).unwrap();
        config["serial"]["port"] = serde_json::json!("/dev/mock");
        config["server"] = serde_json::json!({ "port": 0 });
        config
    }
}

impl SetupPagesWorld for ScopsWorld {
    fn setup_pages(&mut self) -> &mut SetupPagesState {
        &mut self.setup_pages
    }
}

impl ScopsWorld {
    /// Convenience accessor for the Focuser device client.
    pub fn focuser(&self) -> &Arc<dyn Focuser> {
        self.focuser.as_ref().expect("focuser not acquired")
    }

    /// The JSON the suite starts the service on: the world's config (the
    /// library defaults unless a Given changed it, with the mock serial
    /// port in place of the platform default, which the mock ignores) and
    /// the `server` block every scenario shares — port 0 for OS-assigned, no
    /// discovery.
    fn config_json(&self) -> serde_json::Value {
        let mut config = self.config.clone().unwrap_or_default();
        if self.config.is_none() {
            config.serial.port = "/dev/mock".to_string();
        }

        serde_json::json!({
            "serial": {
                "port": config.serial.port,
                "baud_rate": config.serial.baud_rate,
                "polling_interval": format!("{}ms", config.serial.polling_interval.as_millis()),
                "timeout": format!("{}ms", config.serial.timeout.as_millis()),
            },
            "server": {
                "port": 0,
                "discovery_port": null,
            },
            "focuser": {
                "name": config.focuser.name,
                "unique_id": config.focuser.unique_id,
                "description": config.focuser.description,
                "enabled": config.focuser.enabled,
                "max_step": config.focuser.max_step,
            },
        })
    }

    /// Build a JSON config from current world state, write to temp file, and return path.
    fn write_config(&mut self) -> String {
        let config_json = self.config_json();

        let dir = self
            .temp_dir
            .get_or_insert_with(|| TempDir::new().expect("failed to create temp dir"));
        let config_path = dir.path().join("config.json");
        std::fs::write(&config_path, config_json.to_string()).expect("failed to write config");
        config_path.to_str().unwrap().to_string()
    }

    /// Start the focuser service binary and acquire a Focuser client.
    pub async fn start_focuser(&mut self) {
        let config_path = self.write_config();
        let handle = ServiceHandle::start(env!("CARGO_PKG_NAME"), &config_path).await;
        self.adopt(handle, config_path).await;
    }

    /// Spawn the binary with `serial.port` pinned by a `--port` override,
    /// so the server page shows the field pinned and never persists it.
    pub async fn start_with_serial_override(&mut self, serial_port: &str) {
        let config_path = self.write_config();
        let handle = ServiceHandle::start_with_args(
            env!("CARGO_PKG_NAME"),
            &["--config", &config_path, "--port", serial_port],
        )
        .await;
        self.adopt(handle, config_path).await;
    }

    /// Spawn the binary through the TLS + auth fixture, so the setup pages
    /// sit behind the credential.
    pub async fn start_with_tls_and_auth(&mut self) {
        let pki = bdd_infra::tls_auth::shared_pki(env!("CARGO_PKG_NAME")).await;
        self.tls_auth.pki = Some(pki);
        let mut config = self.config_json();
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
        self.focuser_handle = Some(handle);
    }

    /// Record a spawned plain-HTTP service: acquire its device client and
    /// hand the setup-page steps its address and config file.
    async fn adopt(&mut self, handle: ServiceHandle, config_path: String) {
        let focuser = self.acquire_focuser(&handle).await;
        setup_pages::adopt_plain(&mut self.setup_pages, handle.port, config_path.into());
        self.focuser = Some(focuser);
        self.focuser_handle = Some(handle);
    }

    /// Poll until the server returns a Focuser device via the Alpaca client.
    async fn acquire_focuser(&self, handle: &ServiceHandle) -> Arc<dyn Focuser> {
        let addr = SocketAddr::from(([127, 0, 0, 1], handle.port));
        let client = AlpacaClient::new_from_addr(addr);
        for _ in 0..60 {
            tokio::time::sleep(Duration::from_millis(500)).await;
            if let Ok(mut devices) = client.get_devices().await {
                if let Some(TypedDevice::Focuser(focuser)) = devices.next() {
                    return focuser;
                }
            }
        }
        panic!("pa-scops-oag did not become healthy within 30 seconds");
    }
}
