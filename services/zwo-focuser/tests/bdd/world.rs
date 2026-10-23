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
use bdd_infra::setup_pages::{self, SetupPagesState, SetupPagesWorld};
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
    /// The configuration file's contents, when a step stages one; the
    /// default is no override and an OS-assigned port.
    pub staged_config: Option<serde_json::Value>,
    /// The port `--port` pins, when a step pins one.
    pub pinned_port: Option<u16>,

    // Result stashes ("When does, Then asserts").
    pub last_error_code: Option<u16>,
    pub last_actions: Option<Vec<String>>,

    /// State for the shared TLS + auth smoke steps (`auth.feature`).
    pub tls_auth: TlsAuthState,

    /// State for the shared setup-page steps (`setup_pages.feature`).
    pub setup_pages: SetupPagesState,

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

impl SetupPagesWorld for FocuserWorld {
    fn setup_pages(&mut self) -> &mut SetupPagesState {
        &mut self.setup_pages
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
    fn write_config(&mut self) -> std::path::PathBuf {
        let mut config = self
            .staged_config
            .clone()
            .unwrap_or_else(|| serde_json::json!({ "devices": {} }));
        // Port 0 → OS-assigned; the real port is read from the `bound_addr=`
        // line on stdout by ServiceHandle.
        config["server"] = serde_json::json!({ "port": 0 });
        let path = self.scratch_dir().join("zwo-focuser.json");
        std::fs::write(
            &path,
            serde_json::to_string_pretty(&config).expect("serialize config"),
        )
        .expect("write config");
        path
    }

    /// Spawn the service binary, hand its address and config file to the
    /// setup-page steps, and acquire the typed Focuser client.
    pub async fn start(&mut self) {
        let config_path = self.write_config();
        let mut args = vec![
            "--config".to_string(),
            config_path.to_str().expect("utf8 config path").to_string(),
        ];
        if self.empty_backend {
            args.push("--simulation-empty".to_string());
        }
        if let Some(departure) = &self.departure_file {
            args.push("--simulation-departure-file".to_string());
            args.push(departure.to_str().expect("utf8 departure path").to_string());
        }
        if let Some(port) = self.pinned_port {
            args.push("--port".to_string());
            args.push(port.to_string());
        }
        let args: Vec<&str> = args.iter().map(String::as_str).collect();
        let handle = ServiceHandle::start_with_args(env!("CARGO_PKG_NAME"), &args).await;
        setup_pages::adopt_plain(&mut self.setup_pages, handle.bound_port(), config_path);
        self.handle = Some(handle);
        self.acquire().await;
    }

    /// Spawn the binary through the shared TLS + auth fixture, so the setup
    /// pages sit behind the credential.
    pub async fn start_with_tls_and_auth_pages(&mut self) {
        let pki = bdd_infra::tls_auth::shared_pki(env!("CARGO_PKG_NAME")).await;
        self.tls_auth.pki = Some(pki);
        let mut config = serde_json::json!({ "devices": {} });
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
        self.handle = Some(handle);
    }

    async fn acquire(&mut self) {
        let port = self.handle.as_ref().expect("service handle").port();
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

    /// The scenario's scratch directory, created on first use under Bazel's
    /// per-action `TEST_TMPDIR` when there is one (testing.md §5.1): the config
    /// and the departure file live here, read by the service under test.
    pub fn scratch_dir(&mut self) -> std::path::PathBuf {
        self.temp_dir
            .get_or_insert_with(|| {
                bdd_infra::scratch::new_dir("zwo-focuser-bdd-").expect("scratch dir")
            })
            .path()
            .to_path_buf()
    }

    pub fn focuser(&self) -> Arc<dyn Focuser> {
        Arc::clone(self.focuser.as_ref().expect("focuser not acquired"))
    }

    /// The management API answers a `get_devices` request (server is healthy).
    pub async fn management_responds(&self) -> bool {
        let port = self.handle.as_ref().expect("service handle").port();
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
