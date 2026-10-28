//! World struct for Gemini hub Driver BDD tests

use std::sync::Arc;
use std::time::{Duration, Instant};

use ascom_alpaca::api::{ObservingConditions, Switch, TypedDevice};
use ascom_alpaca::{ASCOMError, ASCOMResult, Client};
use bdd_infra::setup_pages::{self, SetupPagesState, SetupPagesWorld};
use bdd_infra::tls_auth::TlsAuthState;
use cucumber::World;

use crate::steps::infrastructure::ServiceHandle;

/// One budget for every wait in this suite (testing.md §5.9 rules 3 and 4).
///
/// It bounds a hang; it asserts nothing about how long a wait *should* take,
/// so it is sized for the slowest leg in the fleet rather than for this box.
/// `safety.yml`'s `ASan` leg puts this suite's ~70 instrumented services through
/// startup at once on a 4-vCPU runner, where spawn-to-ready has been measured
/// with a 74 s tail. Cucumber runs scenarios concurrently, so a generous
/// budget costs the suite one wait, not one per scenario.
const POLL_BUDGET: Duration = Duration::from_secs(90);

/// Gap between attempts inside a wait. Small enough that a wait still gets
/// hundreds of real attempts inside [`POLL_BUDGET`].
const POLL_INTERVAL: Duration = Duration::from_millis(250);

/// Whole-request deadline on the shared HTTP client. Comfortably clears the
/// ~10 s stretches the shared poll loop has been measured unpolled for on the
/// sanitizer leg, while staying a small fraction of [`POLL_BUDGET`] so a
/// stalled call lands in the wait's last-error slot rather than consuming it.
const REQUEST_TIMEOUT: Duration = Duration::from_secs(30);

#[derive(Debug, Default, World)]
pub struct GeminiWorld {
    /// Handle to the running gemini-powerbox-adv3-driver process
    pub gemini: Option<ServiceHandle>,

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
    /// How the last start a scenario expected to be refused ended.
    pub start_outcome: Option<crate::steps::infrastructure::StartOutcome>,
}

impl bdd_infra::doctor_smoke::DoctorSmokeWorld for GeminiWorld {
    fn doctor_smoke(&mut self) -> &mut bdd_infra::doctor_smoke::DoctorSmokeState {
        &mut self.doctor_smoke
    }

    fn valid_config(&self) -> serde_json::Value {
        crate::steps::infrastructure::default_test_config()
    }
}

impl SetupPagesWorld for GeminiWorld {
    fn setup_pages(&mut self) -> &mut SetupPagesState {
        &mut self.setup_pages
    }
}

impl GeminiWorld {
    /// Start the gemini-powerbox-adv3-driver binary with the current config.
    /// Writes config to a temp file, spawns the process, waits for ready,
    /// then discovers devices via the typed ASCOM client.
    pub async fn start_gemini(&mut self) {
        self.start_gemini_with_env(&[]).await;
    }

    /// Start the gemini-powerbox-adv3-driver binary with extra environment variables on the
    /// child process.
    ///
    /// Two facts about the simulated device cannot be reached through the
    /// driver's own command set: the auto-dew mask (the driver never sends
    /// `PD:`) and the overcurrent flags (nothing asks a healthy box to trip a
    /// rail). The mock reads both from its environment, and scenarios run
    /// concurrently in one test process, so the preset has to travel on the
    /// spawned child rather than through `std::env::set_var`.
    pub async fn start_gemini_with_env(&mut self, envs: &[(&str, &str)]) {
        self.start_gemini_with(&[], envs).await;
    }

    /// Start the gemini-powerbox-adv3-driver binary with the current config and `extra`
    /// command-line arguments after `--config <path>` — a `--port` override,
    /// for one, which pins `serial.port` on the setup pages.
    pub async fn start_gemini_with_args(&mut self, extra: &[&str]) {
        self.start_gemini_with(extra, &[]).await;
    }

    async fn start_gemini_with(&mut self, extra: &[&str], envs: &[(&str, &str)]) {
        let config_path = self.write_config().await;
        let mut args = vec!["--config", config_path.as_str()];
        args.extend_from_slice(extra);
        let handle = ServiceHandle::start_with_env(env!("CARGO_PKG_NAME"), &args, envs).await;
        setup_pages::adopt_plain(
            &mut self.setup_pages,
            handle.bound_port(),
            config_path.into(),
        );
        self.adopt(handle).await;
    }

    /// Start the gemini-powerbox-adv3-driver binary through the TLS + auth fixture, so the
    /// setup pages sit behind the credential.
    pub async fn start_gemini_with_tls_and_auth(&mut self) {
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
        self.base_url = Some(handle.base_url());
        self.gemini = Some(handle);
    }

    /// Write the current config to a fresh temp file and return its path.
    async fn write_config(&self) -> String {
        let config_path = std::env::temp_dir().join(format!(
            "gemini-bdd-config-{}.json",
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
        self.base_url = Some(handle.base_url());
        self.gemini = Some(handle);

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
    ///
    /// A transport error is "not yet", not a failure: the child has printed
    /// its bound address but may not be serving yet, and the shared poll loop
    /// (testing.md §5.7) can leave an answered request unread for seconds. The
    /// last error is kept so the panic can say whether the endpoint never
    /// answered or answered with the wrong status.
    async fn wait_for_ready(&self) {
        let base = self.base_url.as_ref().expect("server not started");
        let client = http_client();

        // Try device endpoints and management endpoint (always available)
        let urls = [
            format!("{base}/api/v1/switch/0/name"),
            format!("{base}/api/v1/observingconditions/0/name"),
            format!("{base}/management/apiversions"),
        ];

        let start = Instant::now();
        let mut last_outcome = "no attempt completed".to_string();
        while start.elapsed() < POLL_BUDGET {
            for url in &urls {
                match client.get(url).send().await {
                    Ok(resp) if resp.status().is_success() => return,
                    Ok(resp) => last_outcome = format!("{url} answered {}", resp.status()),
                    Err(e) => last_outcome = format!("{url} failed: {e}"),
                }
            }
            tokio::time::sleep(POLL_INTERVAL).await;
        }
        panic!(
            "gemini-powerbox-adv3-driver did not become ready within {POLL_BUDGET:?} (elapsed {:?}); \
             last outcome: {last_outcome}",
            start.elapsed()
        );
    }

    /// Poll until switch data is available (the status cache has been
    /// populated).
    ///
    /// Reads switch 17, Input Voltage: it is served straight from the frame
    /// cache, so it answers only once a status frame has landed. A writable
    /// switch would not prove that — the driver would still need the cache to
    /// report its state, but a read-only telemetry switch says so plainly.
    pub async fn wait_for_switch_data(&self) {
        const INPUT_VOLTAGE: usize = 17;
        let switch = self.switch.as_ref().expect("switch device not discovered");
        let start = Instant::now();
        let mut last_error = None;
        while start.elapsed() < POLL_BUDGET {
            match switch.get_switch_value(INPUT_VOLTAGE).await {
                Ok(_) => return,
                Err(e) => last_error = Some(e),
            }
            tokio::time::sleep(POLL_INTERVAL).await;
        }
        panic!(
            "switch data did not become available within {POLL_BUDGET:?} (elapsed {:?}); \
             last error: {last_error:?}",
            start.elapsed()
        );
    }

    /// Poll until OC data is available (temperature returns a non-error value).
    pub async fn wait_for_oc_data(&self) {
        let oc = self.oc.as_ref().expect("OC device not discovered");
        let start = Instant::now();
        let mut last_error = None;
        while start.elapsed() < POLL_BUDGET {
            match oc.temperature().await {
                Ok(_) => return,
                Err(e) => last_error = Some(e),
            }
            tokio::time::sleep(POLL_INTERVAL).await;
        }
        panic!(
            "OC data did not become available within {POLL_BUDGET:?} (elapsed {:?}); \
             last error: {last_error:?}",
            start.elapsed()
        );
    }
}

/// The suite's one shared `reqwest` client.
///
/// Built once per test process (testing.md §5.9 rule 2): a fresh client per
/// poll rebuilds a connection pool and pays a new TCP connect every
/// iteration, which is exactly the cost that blows a deadline on a loaded
/// runner. The request timeout keeps a stalled call inside the wait's own
/// budget instead of parking the whole wait in one `await`.
pub fn http_client() -> &'static reqwest::Client {
    static CLIENT: std::sync::OnceLock<reqwest::Client> = std::sync::OnceLock::new();
    CLIENT.get_or_init(|| {
        reqwest::Client::builder()
            .timeout(REQUEST_TIMEOUT)
            .build()
            .expect("failed to build the shared BDD HTTP client")
    })
}

/// Poll `url` with `client` until it answers 200, on the suite's own budget.
///
/// The shape every readiness wait in this suite owes testing.md section 5.9: a
/// wall clock rather than a poll count (rule 3), a failed fetch treated as
/// "not yet" rather than a panic (rule 1), and every attempt bounded by
/// [`REQUEST_TIMEOUT`] so a stalled socket lands in the last-outcome slot
/// instead of parking the whole wait in one `await`.
///
/// The timeout is applied here rather than taken from the client because the
/// TLS scenarios poll with a `PkiFixture` client, which is built to trust a
/// throwaway CA and carries no deadline of its own.
///
/// The final panic distinguishes *the endpoint never answered* from *it
/// answered, but never 200* — rule 1 again, since those point at different
/// bugs.
///
/// # Panics
///
/// Panics if `url` has not answered 200 within [`POLL_BUDGET`].
pub async fn wait_for_http_200(client: &reqwest::Client, url: &str) {
    let start = Instant::now();
    let mut last_outcome = "no attempt completed".to_string();
    while start.elapsed() < POLL_BUDGET {
        match tokio::time::timeout(REQUEST_TIMEOUT, client.get(url).send()).await {
            Ok(Ok(resp)) if resp.status().as_u16() == 200 => return,
            Ok(Ok(resp)) => last_outcome = format!("answered {}", resp.status()),
            Ok(Err(e)) => last_outcome = format!("transport error: {e}"),
            Err(_) => last_outcome = format!("request exceeded {REQUEST_TIMEOUT:?}"),
        }
        tokio::time::sleep(POLL_INTERVAL).await;
    }
    panic!(
        "{url} did not answer 200 within {POLL_BUDGET:?} (elapsed {:?});          last outcome: {last_outcome}",
        start.elapsed()
    );
}
