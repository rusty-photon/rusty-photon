//! World struct for QHY-Focuser BDD tests
//!
//! Uses binary spawning via `ServiceHandle` and HTTP interaction via `AlpacaClient`.

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use ascom_alpaca::api::{Focuser, TypedDevice};
use ascom_alpaca::Client as AlpacaClient;
use bdd_infra::ServiceHandle;
use cucumber::World;
use qhy_focuser::Config;
use tempfile::TempDir;

#[derive(Debug, Default, World)]
pub struct QhyFocuserWorld {
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

    /// Parsed JSON body of the last config.get / config.apply / config.schema action.
    pub last_response: Option<serde_json::Value>,
    /// Result of the last `supported_actions` query.
    pub last_supported_actions: Option<Vec<String>>,

    /// Doctor-subcommand smoke state (staged config file + run output).
    pub doctor_smoke: bdd_infra::doctor_smoke::DoctorSmokeState,
}

impl bdd_infra::doctor_smoke::DoctorSmokeWorld for QhyFocuserWorld {
    fn doctor_smoke(&mut self) -> &mut bdd_infra::doctor_smoke::DoctorSmokeState {
        &mut self.doctor_smoke
    }

    /// `Config::default()` serialised — by construction the full shape the
    /// service's own `deny_unknown_fields` load accepts.
    fn valid_config(&self) -> serde_json::Value {
        serde_json::to_value(Config::default()).expect("config JSON serialisation")
    }
}

impl QhyFocuserWorld {
    /// Convenience accessor for the Focuser device client.
    pub fn focuser(&self) -> &Arc<dyn Focuser> {
        self.focuser.as_ref().expect("focuser not acquired")
    }

    /// Build a JSON config from current world state, write to temp file, and return path.
    fn write_config(&mut self) -> String {
        let config = self.config.clone().unwrap_or_default();

        let config_json = serde_json::json!({
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
                "speed": config.focuser.speed,
                "reverse": config.focuser.reverse,
            },
        });

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
        let focuser = self.acquire_focuser(&handle).await;
        self.focuser = Some(focuser);
        self.focuser_handle = Some(handle);
    }

    /// The OS-assigned port the spawned service bound.
    pub fn bound_port(&self) -> u16 {
        self.focuser_handle
            .as_ref()
            .expect("service not started")
            .port()
    }

    /// Call `config.get`, stash the parsed response, and return the `config`
    /// object so a When step can edit a field and re-`config.apply` it.
    pub async fn current_config(&mut self) -> serde_json::Value {
        let focuser = Arc::clone(self.focuser());
        let body = focuser
            .action("config.get".to_string(), String::new())
            .await
            .expect("config.get failed");
        let parsed: serde_json::Value =
            serde_json::from_str(&body).expect("config.get returned invalid JSON");
        let config = parsed
            .get("config")
            .cloned()
            .expect("config.get response missing `config`");
        self.last_response = Some(parsed);
        config
    }

    /// Call `config.apply` with `params` and stash the parsed response.
    pub async fn call_config_apply(&mut self, params: serde_json::Value) {
        let focuser = Arc::clone(self.focuser());
        let body = focuser
            .action("config.apply".to_string(), params.to_string())
            .await
            .expect("config.apply failed");
        self.last_response =
            Some(serde_json::from_str(&body).expect("config.apply returned invalid JSON"));
    }

    /// Poll `config.get` via a fresh client until `focuser.max_step` equals
    /// `expected`, tolerating the brief blip while the server rebinds. Panics
    /// after ~20 s — which is the point if the reload failed to rebind.
    pub async fn wait_for_config_max_step(&self, expected: u32) {
        let addr = SocketAddr::from(([127, 0, 0, 1], self.bound_port()));
        for _ in 0..80 {
            if try_get_max_step(addr).await == Some(u64::from(expected)) {
                return;
            }
            tokio::time::sleep(Duration::from_millis(250)).await;
        }
        panic!("reloaded service did not report max_step {expected} within 20s");
    }

    /// Poll until the server returns a Focuser device via the Alpaca client.
    async fn acquire_focuser(&self, handle: &ServiceHandle) -> Arc<dyn Focuser> {
        let addr = SocketAddr::from(([127, 0, 0, 1], handle.port()));
        let client = AlpacaClient::new_from_addr(addr);
        for _ in 0..60 {
            tokio::time::sleep(Duration::from_millis(500)).await;
            if let Ok(mut devices) = client.get_devices().await {
                if let Some(TypedDevice::Focuser(focuser)) = devices.next() {
                    return focuser;
                }
            }
        }
        panic!("qhy-focuser did not become healthy within 30 seconds");
    }
}

/// Read `focuser.max_step` from `config.get` via a fresh client, returning
/// `None` on any transport/parse failure (e.g. mid-reload).
async fn try_get_max_step(addr: SocketAddr) -> Option<u64> {
    let client = AlpacaClient::new_from_addr(addr);
    let mut devices = client.get_devices().await.ok()?;
    if let Some(TypedDevice::Focuser(focuser)) = devices.next() {
        let body = focuser
            .action("config.get".to_string(), String::new())
            .await
            .ok()?;
        let parsed: serde_json::Value = serde_json::from_str(&body).ok()?;
        return parsed["config"]["focuser"]["max_step"].as_u64();
    }
    None
}
