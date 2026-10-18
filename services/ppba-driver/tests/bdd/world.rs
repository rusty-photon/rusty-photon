//! World struct for PPBA Driver BDD tests

use std::sync::Arc;
use std::time::Duration;

use ascom_alpaca::api::{ObservingConditions, Switch, TypedDevice};
use ascom_alpaca::{ASCOMError, ASCOMResult, Client};
use bdd_infra::setup_pages::{self, SetupPagesState, SetupPagesWorld};
use bdd_infra::tls_auth::TlsAuthState;
use cucumber::World;

use crate::steps::infrastructure::ServiceHandle;

#[derive(Debug, Default, World)]
pub struct PpbaWorld {
    /// Handle to the running ppba-driver process
    pub ppba: Option<ServiceHandle>,

    /// Base URL of the running server (e.g. "<http://127.0.0.1:12345>")
    pub base_url: Option<String>,

    /// Config JSON built up during Given steps, written to temp file before start
    pub config: serde_json::Value,

    /// Typed ASCOM Switch device client
    pub switch: Option<Arc<dyn Switch>>,

    /// Typed ASCOM `ObservingConditions` device client
    pub oc: Option<Arc<dyn ObservingConditions>>,

    /// ASCOM error from the last "try" operation
    pub last_error: Option<ASCOMError>,

    /// Throwaway PKI + per-run credentials for the TLS/auth scenarios
    pub pki: Option<std::sync::Arc<bdd_infra::tls_auth::PkiFixture>>,

    /// Doctor-subcommand smoke state (staged config file + run output)
    pub doctor_smoke: bdd_infra::doctor_smoke::DoctorSmokeState,

    /// Result of the last `supported_actions` query.
    pub last_supported_actions: Option<Vec<String>>,

    /// State for the shared setup-page steps (`setup_pages.feature`).
    pub setup_pages: SetupPagesState,
    /// The TLS + auth fixture the setup-page credential scenario starts
    /// the service through.
    pub tls_auth: TlsAuthState,

    /// Raw config file text for a scenario that needs a file `config` cannot
    /// express (one that is not JSON at all). Takes precedence over `config`.
    pub config_file_text: Option<String>,
    /// Scratch directory holding the config file a bootstrap scenario staged.
    pub config_dir: Option<tempfile::TempDir>,
    /// The staged config file, and its bytes as written before the driver ran.
    pub config_file: Option<(std::path::PathBuf, Vec<u8>)>,
    /// Exit status and output of a start the driver refused.
    pub refused_start: Option<std::process::Output>,
}

impl bdd_infra::doctor_smoke::DoctorSmokeWorld for PpbaWorld {
    fn doctor_smoke(&mut self) -> &mut bdd_infra::doctor_smoke::DoctorSmokeState {
        &mut self.doctor_smoke
    }

    fn valid_config(&self) -> serde_json::Value {
        crate::steps::infrastructure::default_test_config()
    }
}

impl SetupPagesWorld for PpbaWorld {
    fn setup_pages(&mut self) -> &mut SetupPagesState {
        &mut self.setup_pages
    }
}

impl PpbaWorld {
    /// Start the ppba-driver binary with the current config.
    /// Writes config to a temp file, spawns the process, waits for ready,
    /// then discovers devices via the typed ASCOM client.
    pub async fn start_ppba(&mut self) {
        self.start_ppba_with_args(&[]).await;
    }

    /// Start the ppba-driver binary with the current config and `extra`
    /// command-line arguments after `--config <path>` — a `--port` override,
    /// for one, which pins `serial.port` on the setup pages.
    pub async fn start_ppba_with_args(&mut self, extra: &[&str]) {
        let config_path = self.write_config().await;
        let mut args = vec!["--config", config_path.as_str()];
        args.extend_from_slice(extra);
        let handle = ServiceHandle::start_with_args(env!("CARGO_PKG_NAME"), &args).await;
        setup_pages::adopt_plain(&mut self.setup_pages, handle.port, config_path.into());
        self.adopt(handle).await;
    }

    /// Start the ppba-driver binary through the TLS + auth fixture, so the
    /// setup pages sit behind the credential.
    pub async fn start_ppba_with_tls_and_auth(&mut self) {
        let pki = bdd_infra::tls_auth::shared_pki(env!("CARGO_PKG_NAME")).await;
        self.tls_auth.pki = Some(pki);
        let mut config = self.config.clone();
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
        self.base_url = Some(handle.base_url.clone());
        self.ppba = Some(handle);
    }

    /// Write the current config to a fresh temp file and return its path.
    async fn write_config(&self) -> String {
        let config_path = std::env::temp_dir().join(format!(
            "ppba-bdd-config-{}.json",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        tokio::fs::write(
            &config_path,
            serde_json::to_string_pretty(&self.config).unwrap(),
        )
        .await
        .expect("failed to write test config");
        config_path.to_str().unwrap().to_string()
    }

    /// Record a spawned plain-HTTP service: wait for it, then discover its
    /// devices through the typed ASCOM client.
    async fn adopt(&mut self, handle: ServiceHandle) {
        self.base_url = Some(handle.base_url.clone());
        self.ppba = Some(handle);

        // Wait for the server to be ready
        self.wait_for_ready().await;

        // Discover devices via typed ASCOM client, retrying while the
        // service's Alpaca routes come up.
        let base_url = self.base_url.as_ref().unwrap();
        for attempt in 0..20 {
            let client = Client::new(base_url).unwrap();
            match client.get_devices().await {
                Ok(devices) => {
                    for device in devices {
                        #[allow(unreachable_patterns)]
                        match device {
                            TypedDevice::Switch(s) => self.switch = Some(s),
                            TypedDevice::ObservingConditions(oc) => self.oc = Some(oc),
                            _ => {}
                        }
                    }
                    return;
                }
                Err(_) if attempt < 19 => {
                    tokio::time::sleep(Duration::from_millis(100)).await;
                }
                Err(e) => panic!("Failed to discover devices after 20 attempts: {e}"),
            }
        }
    }

    /// Get a reference to the typed Switch device.
    pub fn switch_ref(&self) -> &dyn Switch {
        self.switch
            .as_ref()
            .expect("switch device not discovered")
            .as_ref()
    }

    /// Get a reference to the typed `ObservingConditions` device.
    pub fn oc_ref(&self) -> &dyn ObservingConditions {
        self.oc.as_ref().expect("OC device not discovered").as_ref()
    }

    /// Capture an ASCOM result: store the error on Err, clear on Ok.
    pub fn capture_result<T>(&mut self, result: ASCOMResult<T>) {
        match result {
            Ok(_) => self.last_error = None,
            Err(e) => self.last_error = Some(e),
        }
    }

    /// Poll until the server is ready to accept requests.
    /// Tries device endpoints and management endpoint until HTTP 200.
    async fn wait_for_ready(&self) {
        let base = self.base_url.as_ref().expect("server not started");
        let client = reqwest::Client::new();

        // Try device endpoints and management endpoint (always available)
        let urls = [
            format!("{base}/api/v1/switch/0/name"),
            format!("{base}/api/v1/observingconditions/0/name"),
            format!("{base}/management/apiversions"),
        ];

        for _ in 0..120 {
            for url in &urls {
                if let Ok(resp) = client.get(url).send().await {
                    if resp.status().is_success() {
                        return;
                    }
                }
            }
            tokio::time::sleep(Duration::from_millis(250)).await;
        }
        panic!("ppba-driver did not become ready within 30 seconds");
    }

    /// Poll until switch data is available (the status cache has been populated).
    pub async fn wait_for_switch_data(&self) {
        let switch = self.switch.as_ref().expect("switch device not discovered");
        for _ in 0..120 {
            if switch.get_switch_value(10).await.is_ok() {
                return;
            }
            tokio::time::sleep(Duration::from_millis(250)).await;
        }
        panic!("switch data did not become available within 30 seconds");
    }

    /// Poll until OC data is available (temperature returns a non-error value).
    pub async fn wait_for_oc_data(&self) {
        let oc = self.oc.as_ref().expect("OC device not discovered");
        for _ in 0..120 {
            if oc.temperature().await.is_ok() {
                return;
            }
            tokio::time::sleep(Duration::from_millis(250)).await;
        }
        panic!("OC data did not become available within 30 seconds");
    }
}
