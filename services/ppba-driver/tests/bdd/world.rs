//! World struct for PPBA Driver BDD tests

use std::sync::Arc;
use std::time::Duration;

use ascom_alpaca::api::{ObservingConditions, Switch, TypedDevice};
use ascom_alpaca::{ASCOMError, ASCOMResult, Client};
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

    /// Parsed JSON body of the last config.get / config.apply / config.schema action.
    pub last_response: Option<serde_json::Value>,
    /// Result of the last `supported_actions` query.
    pub last_supported_actions: Option<Vec<String>>,
}

impl bdd_infra::doctor_smoke::DoctorSmokeWorld for PpbaWorld {
    fn doctor_smoke(&mut self) -> &mut bdd_infra::doctor_smoke::DoctorSmokeState {
        &mut self.doctor_smoke
    }

    fn valid_config(&self) -> serde_json::Value {
        crate::steps::infrastructure::default_test_config()
    }
}

impl PpbaWorld {
    /// Start the ppba-driver binary with the current config.
    /// Writes config to a temp file, spawns the process, waits for ready,
    /// then discovers devices via the typed ASCOM client.
    pub async fn start_ppba(&mut self) {
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

        let handle =
            ServiceHandle::start(env!("CARGO_PKG_NAME"), config_path.to_str().unwrap()).await;
        self.base_url = Some(handle.base_url.clone());
        self.ppba = Some(handle);

        // Wait for the server to be ready
        self.wait_for_ready().await;

        // Discover devices via typed ASCOM client.
        // Creates a fresh Client on each attempt because the random ClientID
        // may exceed i32::MAX, which the server rejects with 400 (it parses
        // integers as i32 per ASCOM spec). Retrying gives a fresh random ID.
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

    /// The OS-assigned port the spawned service bound.
    pub const fn bound_port(&self) -> u16 {
        self.ppba.as_ref().expect("service not started").port
    }

    /// Call `config.get` on the switch device, stash the parsed response, and
    /// return the `config` object so a When step can edit it and re-apply.
    pub async fn current_config(&mut self) -> serde_json::Value {
        let switch = Arc::clone(self.switch.as_ref().expect("switch not discovered"));
        let body = switch
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

    /// Call `config.apply` on the switch device with `params`, stashing the
    /// parsed response.
    pub async fn call_config_apply(&mut self, params: serde_json::Value) {
        let switch = Arc::clone(self.switch.as_ref().expect("switch not discovered"));
        let body = switch
            .action("config.apply".to_string(), params.to_string())
            .await
            .expect("config.apply failed");
        self.last_response =
            Some(serde_json::from_str(&body).expect("config.apply returned invalid JSON"));
    }

    /// Poll `config.get` via a fresh client until `switch.name` equals
    /// `expected`, tolerating the brief blip while the server rebinds.
    pub async fn wait_for_config_switch_name(&self, expected: &str) {
        self.wait_for_config_value("/switch/name", &serde_json::json!(expected))
            .await;
    }

    /// Poll `config.get` via a fresh client until the JSON pointer `pointer`
    /// is absent from the served config, tolerating the brief blip while the
    /// server rebinds.
    pub async fn wait_for_config_key_absent(&self, pointer: &str) {
        let base_url = self.base_url.as_ref().expect("server not started").clone();
        let mut last_seen = None;
        for _ in 0..80 {
            // A server that is still rebinding answers nothing at all, which
            // reads the same as "the key is gone" — so the absence only counts
            // once `config.get` has answered with a config that lacks it.
            match try_get_config(&base_url).await {
                Some(config) if config.pointer(pointer).is_none() => return,
                seen => last_seen = seen,
            }
            tokio::time::sleep(Duration::from_millis(250)).await;
        }
        panic!(
            "reloaded service still served {pointer} after 20s; last config seen: {last_seen:?}"
        );
    }

    /// Poll a fresh client, connecting each attempt, until `GetSwitchName(id)`
    /// equals `expected`. The reload rebuilds the device, so the client that
    /// discovered the old one cannot answer for the new one.
    pub async fn wait_for_switch_name(&self, id: usize, expected: &str) {
        let base_url = self.base_url.as_ref().expect("server not started").clone();
        let mut last_seen = None;
        for _ in 0..80 {
            last_seen = try_get_switch_name(&base_url, id).await;
            if last_seen.as_deref() == Some(expected) {
                return;
            }
            tokio::time::sleep(Duration::from_millis(250)).await;
        }
        panic!(
            "reloaded service did not name switch {id} {expected} within 20s; \
             last name seen: {last_seen:?}"
        );
    }

    /// Poll `config.get` via a fresh client until the JSON pointer `pointer`
    /// into the served config equals `expected`, tolerating the brief blip
    /// while the server rebinds. Panics after ~20 s — which is the point if
    /// the reload failed to rebind.
    pub async fn wait_for_config_value(&self, pointer: &str, expected: &serde_json::Value) {
        let base_url = self.base_url.as_ref().expect("server not started").clone();
        let mut last_seen = None;
        for _ in 0..80 {
            last_seen = try_get_config_value(&base_url, pointer).await;
            if last_seen.as_ref() == Some(expected) {
                return;
            }
            tokio::time::sleep(Duration::from_millis(250)).await;
        }
        panic!(
            "reloaded service did not report {pointer} as {expected} within 20s; \
             last value seen: {last_seen:?}"
        );
    }

    /// Call `config.apply` on the switch device with `params`, stashing the
    /// ASCOM error instead of panicking when the submission is refused
    /// outright.
    ///
    /// A config blob the driver cannot even deserialize — a label map that
    /// breaks the switch table, say — comes back as an `INVALID_VALUE` error
    /// rather than as a response carrying field errors, so the "try" shape is
    /// the only one that can observe it (testing.md section 3.5).
    pub async fn try_config_apply(&mut self, params: serde_json::Value) {
        let switch = Arc::clone(self.switch.as_ref().expect("switch not discovered"));
        match switch
            .action("config.apply".to_string(), params.to_string())
            .await
        {
            Ok(body) => {
                self.last_error = None;
                self.last_response =
                    Some(serde_json::from_str(&body).expect("config.apply returned invalid JSON"));
            }
            Err(e) => self.last_error = Some(e),
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

/// Read the served config out of `config.get` via a fresh client, returning
/// `None` on any transport/parse failure (e.g. mid-reload).
async fn try_get_config(base_url: &str) -> Option<serde_json::Value> {
    let client = Client::new(base_url).ok()?;
    let devices = client.get_devices().await.ok()?;
    for device in devices {
        if let TypedDevice::Switch(s) = device {
            let body = s
                .action("config.get".to_string(), String::new())
                .await
                .ok()?;
            let parsed: serde_json::Value = serde_json::from_str(&body).ok()?;
            return parsed.get("config").cloned();
        }
    }
    None
}

/// Read the JSON pointer `pointer` out of `config.get`'s served config via a
/// fresh client, returning `None` on any transport/parse failure (e.g.
/// mid-reload) or when the key is absent.
async fn try_get_config_value(base_url: &str, pointer: &str) -> Option<serde_json::Value> {
    try_get_config(base_url).await?.pointer(pointer).cloned()
}

/// Read `GetSwitchName(id)` via a fresh client, connecting it first, and
/// returning `None` on any failure (e.g. mid-reload). Connecting is safe to
/// repeat: the handshake is read-only and `set_connected` is idempotent.
async fn try_get_switch_name(base_url: &str, id: usize) -> Option<String> {
    let client = Client::new(base_url).ok()?;
    let devices = client.get_devices().await.ok()?;
    for device in devices {
        if let TypedDevice::Switch(s) = device {
            s.set_connected(true).await.ok()?;
            return s.get_switch_name(id).await.ok();
        }
    }
    None
}
