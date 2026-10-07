//! Cucumber `World` for the zwo-focuser BDD suite.
//!
//! Each scenario spawns the zwo-focuser binary (built with the `simulation`
//! backend so the SDK yields one `EAF-Simulated` focuser) and drives it
//! through the typed `ascom-alpaca` Focuser client over real HTTP — mirroring
//! the zwo-camera / qhy-focuser pattern.

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use ascom_alpaca::api::{Focuser, TypedDevice};
use ascom_alpaca::ASCOMErrorCode;
use ascom_alpaca::Client as AlpacaClient;
use bdd_infra::tls_auth::{TlsAuthSmokeWorld, TlsAuthState};
use bdd_infra::ServiceHandle;
use cucumber::World;
use tempfile::TempDir;

#[derive(Debug, Default, World)]
pub struct FocuserWorld {
    pub handle: Option<ServiceHandle>,
    pub focuser: Option<Arc<dyn Focuser>>,
    pub temp_dir: Option<TempDir>,

    // Config knobs set by a Given step before the service starts.
    pub empty_backend: bool,
    /// While this file exists the simulated EAF is off the bus (C5).
    pub departure_file: Option<std::path::PathBuf>,

    // Result stashes ("When does, Then asserts").
    pub last_error_code: Option<u16>,
    pub last_response: Option<serde_json::Value>,
    pub last_actions: Option<Vec<String>>,

    /// State for the shared TLS + auth smoke steps (`auth.feature`).
    pub tls_auth: TlsAuthState,

    /// Doctor-subcommand smoke state (staged config file + run output).
    pub doctor_smoke: bdd_infra::doctor_smoke::DoctorSmokeState,
}

impl bdd_infra::doctor_smoke::DoctorSmokeWorld for FocuserWorld {
    fn doctor_smoke(&mut self) -> &mut bdd_infra::doctor_smoke::DoctorSmokeState {
        &mut self.doctor_smoke
    }

    fn valid_config(&self) -> serde_json::Value {
        serde_json::json!({ "devices": {}, "server": { "port": 0 } })
    }
}

impl TlsAuthSmokeWorld for FocuserWorld {
    fn tls_auth(&mut self) -> &mut TlsAuthState {
        &mut self.tls_auth
    }

    fn base_test_config(&self) -> serde_json::Value {
        serde_json::json!({ "devices": {} })
    }

    async fn start_with_tls_auth(&mut self, config: serde_json::Value) {
        let handle = bdd_infra::tls_auth::spawn_service_handle(
            &mut self.tls_auth,
            env!("CARGO_PKG_NAME"),
            &config,
        )
        .await;
        self.handle = Some(handle);
    }
}

impl FocuserWorld {
    fn write_config(&mut self) -> String {
        let config = serde_json::json!({
            "devices": {},
            // Port 0 → OS-assigned; the real port is read from the `bound_addr=`
            // line on stdout by ServiceHandle.
            "server": { "port": 0 },
        });
        let dir = self
            .temp_dir
            .get_or_insert_with(|| TempDir::new().expect("temp dir"));
        let path = dir.path().join("zwo-focuser.json");
        std::fs::write(
            &path,
            serde_json::to_string_pretty(&config).expect("serialize config"),
        )
        .expect("write config");
        path.to_str().expect("utf8 config path").to_string()
    }

    /// Spawn the service binary and acquire the typed Focuser client.
    pub async fn start(&mut self) {
        let config_path = self.write_config();
        let handle = if self.empty_backend {
            ServiceHandle::start_with_args(
                env!("CARGO_PKG_NAME"),
                &["--config", &config_path, "--simulation-empty"],
            )
            .await
        } else if let Some(departure) = &self.departure_file {
            let departure = departure.to_str().expect("utf8 departure path");
            ServiceHandle::start_with_args(
                env!("CARGO_PKG_NAME"),
                &[
                    "--config",
                    &config_path,
                    "--simulation-departure-file",
                    departure,
                ],
            )
            .await
        } else {
            ServiceHandle::start(env!("CARGO_PKG_NAME"), &config_path).await
        };
        self.handle = Some(handle);
        self.acquire().await;
    }

    async fn acquire(&mut self) {
        let port = self.handle.as_ref().expect("service handle").port;
        let addr = SocketAddr::from(([127, 0, 0, 1], port));
        for _ in 0..80 {
            let client = AlpacaClient::new_from_addr(addr);
            if let Ok(devices) = client.get_devices().await {
                let mut focuser = None;
                for device in devices {
                    // A `match` (not `if let`) mirrors zwo-camera, in case a
                    // future device type joins `TypedDevice`.
                    #[allow(clippy::single_match)]
                    match device {
                        TypedDevice::Focuser(f) => focuser = Some(f),
                        #[allow(unreachable_patterns)]
                        _ => {}
                    }
                }
                if self.empty_backend {
                    // Zero focusers is the expected, healthy state here (C0).
                    self.focuser = focuser;
                    return;
                }
                if focuser.is_some() {
                    self.focuser = focuser;
                    return;
                }
            }
            tokio::time::sleep(Duration::from_millis(250)).await;
        }
        assert!(
            self.empty_backend,
            "zwo-focuser did not register a Focuser device within 20s"
        );
    }

    /// The scenario's scratch directory (the config and the departure file
    /// live here).
    pub fn scratch_dir(&mut self) -> std::path::PathBuf {
        self.temp_dir
            .get_or_insert_with(|| TempDir::new().expect("temp dir"))
            .path()
            .to_path_buf()
    }

    pub fn focuser(&self) -> Arc<dyn Focuser> {
        Arc::clone(self.focuser.as_ref().expect("focuser not acquired"))
    }

    /// The management API answers a `get_devices` request (server is healthy).
    pub async fn management_responds(&self) -> bool {
        let port = self.handle.as_ref().expect("service handle").port;
        let addr = SocketAddr::from(([127, 0, 0, 1], port));
        AlpacaClient::new_from_addr(addr)
            .get_devices()
            .await
            .is_ok()
    }

    /// Drive a `Move` and stash the ASCOM error code (`None` on success).
    pub async fn try_move(&mut self, position: i32) {
        match self.focuser().move_(position).await {
            Ok(()) => self.last_error_code = None,
            Err(e) => self.last_error_code = Some(e.code.raw()),
        }
    }

    /// Drive a `Halt` and stash the ASCOM error code (`None` on success).
    pub async fn try_halt(&mut self) {
        match self.focuser().halt().await {
            Ok(()) => self.last_error_code = None,
            Err(e) => self.last_error_code = Some(e.code.raw()),
        }
    }

    /// Query `Position` and stash the ASCOM error code (`None` on success) —
    /// used to exercise the disconnected-rejection path (M14).
    pub async fn try_position(&mut self) {
        match self.focuser().position().await {
            Ok(_) => self.last_error_code = None,
            Err(e) => self.last_error_code = Some(e.code.raw()),
        }
    }

    /// Query `StepSize` and stash the ASCOM error code (always `NOT_IMPLEMENTED`).
    pub async fn try_step_size(&mut self) {
        match self.focuser().step_size().await {
            Ok(_) => self.last_error_code = None,
            Err(e) => self.last_error_code = Some(e.code.raw()),
        }
    }

    /// Drive a `SetTempComp` and stash the ASCOM error code (always
    /// `NOT_IMPLEMENTED`).
    pub async fn try_set_temp_comp(&mut self, value: bool) {
        match self.focuser().set_temp_comp(value).await {
            Ok(()) => self.last_error_code = None,
            Err(e) => self.last_error_code = Some(e.code.raw()),
        }
    }

    /// Call a vendor config action; stash the parsed JSON (`last_response`) on
    /// success, or the ASCOM error code (`last_error_code`) on failure.
    pub async fn call_action(&mut self, action: &str, params: &str) {
        match self
            .focuser()
            .action(action.to_string(), params.to_string())
            .await
        {
            Ok(body) => {
                self.last_error_code = None;
                self.last_response =
                    Some(serde_json::from_str(&body).expect("action returned invalid JSON"));
            }
            Err(e) => {
                self.last_error_code = Some(e.code.raw());
                self.last_response = None;
            }
        }
    }

    /// The `config` object from a `config.get` response.
    pub async fn config_get(&mut self) -> serde_json::Value {
        self.call_action("config.get", "").await;
        self.last_response
            .as_ref()
            .and_then(|r| r.get("config").cloned())
            .expect("config.get response missing `config`")
    }
}

/// Map an ASCOM error-code *name* (as written in the feature files) to its raw
/// `u16`, so Then steps can assert "rejected with ASCOM <NAME>".
pub fn ascom_code(name: &str) -> u16 {
    match name {
        "INVALID_VALUE" => ASCOMErrorCode::INVALID_VALUE.raw(),
        "NOT_CONNECTED" => ASCOMErrorCode::NOT_CONNECTED.raw(),
        "NOT_IMPLEMENTED" => ASCOMErrorCode::NOT_IMPLEMENTED.raw(),
        "INVALID_OPERATION" => ASCOMErrorCode::INVALID_OPERATION.raw(),
        other => panic!("unknown ASCOM error code name: {other}"),
    }
}
