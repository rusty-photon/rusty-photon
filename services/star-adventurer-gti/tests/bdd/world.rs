//! World struct for star-adventurer-gti BDD tests.
//!
//! Drives a real `star-adventurer-gti` binary spawned via
//! `bdd_infra::ServiceHandle`. The binary runs with `--features mock`,
//! which routes its `TransportFactory` through `CapturingMockFactory`
//! and mounts `/debug/v1/mock-commands` so step assertions about wire
//! frames can fetch the mock's command log over HTTP.

#![allow(dead_code)]

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::{Duration, Instant};

use ascom_alpaca::api::telescope::{PierSide, Telescope};
use ascom_alpaca::api::TypedDevice;
use ascom_alpaca::ASCOMError;
use ascom_alpaca::Client as AlpacaClient;
use bdd_infra::tls_auth::{TlsAuthSmokeWorld, TlsAuthState};
use bdd_infra::ServiceHandle;
use cucumber::World;
use serde_json::Value;
use star_adventurer_gti::{
    AlpacaServerConfig, ApPark, Config, CwExclusionZone, MinAltitudeDegrees, MountConfig,
    TransportConfig, UsbConfig,
};
use tempfile::TempDir;
use tokio::time::sleep;

/// Whole-request budget — connect, headers and response body — for one
/// call to a `/debug/v1/*` mock-introspection endpoint.
const DEBUG_REQUEST_TIMEOUT: Duration = Duration::from_secs(15);

/// How long the mock-introspection helpers keep retrying before they give
/// up and fail the scenario.
pub const DEBUG_RETRY_WINDOW: Duration = Duration::from_secs(20);

/// Gap between mock-introspection polls.
const DEBUG_POLL_INTERVAL: Duration = Duration::from_millis(100);

/// Suite-wide HTTP client for the `/debug/v1/*` mock-introspection
/// endpoints.
///
/// One pooled client for the whole test process rather than one per call:
/// the command-log steps poll in a loop, so a client per call would
/// rebuild a connection pool — and pay a fresh TCP connect — on every
/// poll. The timeout is a whole-request deadline covering connect,
/// headers and body, sized for a runner where a request can sit
/// mid-response for seconds: every scenario shares one poll loop, so any
/// step that blocks stops this fetch too (`docs/skills/testing.md` §5.7),
/// and dozens of this suite's service processes compete for a few cores.
fn debug_client() -> reqwest::Client {
    static CLIENT: std::sync::OnceLock<reqwest::Client> = std::sync::OnceLock::new();
    CLIENT
        .get_or_init(|| {
            reqwest::Client::builder()
                .timeout(DEBUG_REQUEST_TIMEOUT)
                .build()
                .expect("reqwest client builds")
        })
        .clone()
}

/// Why a [`StarAdventurerWorld::wait_for_command_log`] wait ended without
/// a match.
///
/// The two are worth telling apart in a panic message: `NoMatch` means the
/// driver really never emitted the frame, `FetchFailed` means the
/// assertion never got the chance to run.
#[derive(Debug)]
pub enum CommandLogTimeout {
    /// The endpoint answered, but the predicate never accepted the log.
    /// Carries the last log read.
    NoMatch(Vec<String>),
    /// No read of the log ever succeeded — the endpoint was unreachable,
    /// answered non-2xx, or never finished inside the window. Carries the
    /// last failure, whose text names which.
    FetchFailed(String),
}

/// Index of the first frame after the most recent startup block.
///
/// A startup block is the identity-gated handshake — which always opens
/// with `:e1`, a frame nothing else on this wire sends — followed by the
/// cold start's `:L1`, `:L2`, `:K1` safety assertion. Scanning for the
/// *last* `:e1` and stepping past the halt behind it yields the point
/// after which every frame belongs to the scenario rather than to the
/// driver coming up, however many times it has come up.
///
/// Returns 0 when no startup is present (nothing has booted yet), and
/// the position after the last `:e1` when the halt behind it has not
/// been logged yet — mid-boot, where dropping the handshake is still
/// right and the halt is about to follow.
fn end_of_last_startup(log: &[String]) -> usize {
    let Some(boot) = log.iter().rposition(|c| c.as_str() == ":e1\r") else {
        return 0;
    };
    let mut idx = boot + 1;
    for want in [":L1\r", ":L2\r", ":K1\r"] {
        match log[idx..].iter().position(|c| c.as_str() == want) {
            Some(hit) => idx += hit + 1,
            None => return boot + 1,
        }
    }
    idx
}

#[derive(Debug, Default, World)]
pub struct StarAdventurerWorld {
    pub service_handle: Option<ServiceHandle>,
    pub mount: Option<Arc<dyn Telescope>>,
    pub config: Option<Config>,
    pub temp_dir: Option<TempDir>,
    pub last_error: Option<String>,
    pub last_error_code: Option<u16>,
    /// Last successful `DestinationSideOfPier` result. Set by the
    /// "I read `DestinationSideOfPier` ..." step so the matching
    /// `Then DestinationSideOfPier should be ...` step can assert on
    /// it without re-issuing the wire call.
    pub last_destination_pier_side: Option<PierSide>,
    /// Pending state-seed body that BDD `Given` steps build up before
    /// the service is started. After `start_service` spawns the binary,
    /// `apply_pending_seed()` POSTs this to `/debug/v1/mock-state` so
    /// the mount has the desired state before the first `When I
    /// connect` step.
    ///
    /// Persists across applies so a scenario that re-spawns the
    /// binary (e.g. a `Given service configured with X` followed by
    /// a `Given a running service`) keeps the seeds for the second
    /// spawn. Each `queue_seed` call overwrites by key, so the map
    /// always carries the latest desired state.
    pub pending_seed: serde_json::Map<String, serde_json::Value>,

    /// Parsed JSON body of the last config.get / config.apply / config.schema action.
    pub last_response: Option<Value>,
    /// Result of the last `supported_actions` query.
    pub last_supported_actions: Option<Vec<String>>,

    /// State for the shared TLS + auth smoke steps (`auth.feature`).
    pub tls_auth: TlsAuthState,

    /// Doctor-subcommand smoke state (staged config file + run output).
    pub doctor_smoke: bdd_infra::doctor_smoke::DoctorSmokeState,
}

impl bdd_infra::doctor_smoke::DoctorSmokeWorld for StarAdventurerWorld {
    fn doctor_smoke(&mut self) -> &mut bdd_infra::doctor_smoke::DoctorSmokeState {
        &mut self.doctor_smoke
    }

    /// The tls-auth smoke's base config is a full serialised [`Config`]
    /// (`server` block included), so it is already the full shape the
    /// service's own `deny_unknown_fields` load accepts.
    fn valid_config(&self) -> serde_json::Value {
        self.base_test_config()
    }
}

impl TlsAuthSmokeWorld for StarAdventurerWorld {
    fn tls_auth(&mut self) -> &mut TlsAuthState {
        &mut self.tls_auth
    }

    /// The world's default test config (mock serial transport, port 0,
    /// BDD-safe mount envelope). It serialises with a plain `server` block,
    /// which the shared configure step replaces with the TLS + auth one.
    fn base_test_config(&self) -> serde_json::Value {
        let config = self.config.clone().unwrap_or_else(default_test_config);
        serde_json::to_value(&config).expect("config JSON serialisation")
    }

    async fn start_with_tls_auth(&mut self, config: serde_json::Value) {
        let handle = bdd_infra::tls_auth::spawn_service_handle(
            &mut self.tls_auth,
            env!("CARGO_PKG_NAME"),
            &config,
        )
        .await;
        self.service_handle = Some(handle);
    }
}

impl StarAdventurerWorld {
    pub fn mount(&self) -> &Arc<dyn Telescope> {
        self.mount
            .as_ref()
            .expect("mount client not acquired — did the service start?")
    }

    /// Mutable accessor for the in-flight [`Config`]. Step bodies that
    /// tweak knobs (`a star-adventurer service configured with site
    /// latitude N`) call this before [`start_service`].
    pub fn config_mut(&mut self) -> &mut Config {
        self.config.get_or_insert_with(default_test_config)
    }

    /// Build a JSON config from current world state, write it to a temp
    /// file, spawn the service binary via [`ServiceHandle`], poll the
    /// Alpaca client until the Telescope device is exposed, and apply
    /// any deferred state seeds that earlier `Given` steps queued up.
    pub async fn start_service(&mut self) {
        let cfg = self.config.clone().unwrap_or_else(default_test_config);
        let dir = self
            .temp_dir
            .get_or_insert_with(|| TempDir::new().expect("failed to create temp dir"));
        let config_path = dir.path().join("config.json");
        let json = serde_json::to_string_pretty(&cfg).expect("config JSON serialise");
        std::fs::write(&config_path, json).expect("write config");
        let path_str = config_path.to_str().expect("UTF-8 temp path").to_string();

        let handle = ServiceHandle::start(env!("CARGO_PKG_NAME"), &path_str).await;
        let mount = acquire_mount(&handle).await;
        self.config = Some(cfg);
        self.mount = Some(mount);
        self.service_handle = Some(handle);
        self.apply_pending_seed().await;
    }

    /// The OS-assigned port the spawned service bound.
    pub const fn bound_port(&self) -> u16 {
        self.service_handle
            .as_ref()
            .expect("service not started")
            .port
    }

    /// Call `config.get`, stash the parsed response, and return the `config`
    /// object so a When step can edit it and re-apply.
    pub async fn current_config(&mut self) -> Value {
        let mount = Arc::clone(self.mount());
        let body = mount
            .action("config.get".to_string(), String::new())
            .await
            .expect("config.get failed");
        let parsed: Value = serde_json::from_str(&body).expect("config.get returned invalid JSON");
        let config = parsed
            .get("config")
            .cloned()
            .expect("config.get response missing `config`");
        self.last_response = Some(parsed);
        config
    }

    /// Call `config.apply` with `params`, stashing the parsed response.
    pub async fn call_config_apply(&mut self, params: Value) {
        let mount = Arc::clone(self.mount());
        let body = mount
            .action("config.apply".to_string(), params.to_string())
            .await
            .expect("config.apply failed");
        self.last_response =
            Some(serde_json::from_str(&body).expect("config.apply returned invalid JSON"));
    }

    /// Poll `config.get` via a fresh client until `mount.description` equals
    /// `expected`, tolerating the brief blip while the server rebinds.
    pub async fn wait_for_config_description(&self, expected: &str) {
        let addr = std::net::SocketAddr::from(([127, 0, 0, 1], self.bound_port()));
        for _ in 0..80 {
            if try_get_mount_description(addr).await.as_deref() == Some(expected) {
                return;
            }
            sleep(std::time::Duration::from_millis(250)).await;
        }
        panic!("reloaded service did not report description {expected} within 20s");
    }

    /// POST the queued state seed to `/debug/v1/mock-state`. No-op when
    /// no seed has been queued.
    ///
    /// Does NOT clear `pending_seed` — scenarios sometimes re-spawn the
    /// binary (e.g. a `Given service configured with name X` followed
    /// by another `Given a running service`) and the new binary needs
    /// the same seed. Each `queue_seed` overwrites by key, so the map
    /// always holds the latest desired state; re-applying is
    /// idempotent.
    pub async fn apply_pending_seed(&mut self) {
        if self.pending_seed.is_empty() {
            return;
        }
        let handle = self
            .service_handle
            .as_ref()
            .expect("service not started — cannot apply seed");
        let url = format!("http://127.0.0.1:{}/debug/v1/mock-state", handle.port);
        let body = serde_json::Value::Object(self.pending_seed.clone());
        // Retry transport failures for the same reason
        // `wait_for_command_log` does: a request that loses the scheduler
        // race on a loaded runner must not fail the scenario. A 4xx is the
        // mock rejecting the seed itself — deterministic, so it surfaces
        // immediately instead.
        let deadline = Instant::now() + DEBUG_RETRY_WINDOW;
        let mut last_err;
        loop {
            match debug_client().post(&url).json(&body).send().await {
                Ok(resp) => {
                    let status = resp.status();
                    if status.is_success() {
                        return;
                    }
                    assert!(!status.is_client_error(), "seed POST rejected: {status}");
                    last_err = format!("seed POST failed: {status}");
                }
                Err(e) => last_err = format!("{e:?}"),
            }
            assert!(
                Instant::now() < deadline,
                "seed endpoint unreachable within {DEBUG_RETRY_WINDOW:?}: {last_err}"
            );
            sleep(DEBUG_POLL_INTERVAL).await;
        }
    }

    /// Queue a single seed value to be `POSTed` to `/debug/v1/mock-state`.
    ///
    /// If the service is already running, the seed is applied
    /// immediately — this lets `Given` steps that follow "a running
    /// star-adventurer service" still pre-set mock state before the
    /// next `When I connect` runs. Otherwise the seed accumulates and
    /// gets flushed on the next `start_service`.
    pub async fn queue_seed(&mut self, key: &str, value: serde_json::Value) {
        self.pending_seed.insert(key.to_string(), value);
        if self.service_handle.is_some() {
            self.apply_pending_seed().await;
        }
    }

    /// One attempt at reading the mock-mode wire-command log from the
    /// running service's `/debug/v1/mock-commands` endpoint. Returns each
    /// frame as a `String` (the wire protocol is ASCII, so the conversion
    /// is lossless).
    ///
    /// Transport and status failures come back as `Err` so callers can
    /// retry them. A body that parses but has the wrong shape still
    /// panics: that is a contract break, and retrying it would only
    /// convert a clear failure into a timeout.
    pub async fn try_command_log(&self) -> Result<Vec<String>, String> {
        let handle = self
            .service_handle
            .as_ref()
            .expect("service not started — call start_service first");
        let url = format!("http://127.0.0.1:{}/debug/v1/mock-commands", handle.port);
        let body: Value = debug_client()
            .get(&url)
            .send()
            .await
            .map_err(|e| format!("{e:?}"))?
            .error_for_status()
            .map_err(|e| format!("{e:?}"))?
            .json()
            .await
            .map_err(|e| format!("{e:?}"))?;
        Ok(body["commands"]
            .as_array()
            .expect("commands is an array")
            .iter()
            .map(|v| v.as_str().expect("frame is a string").to_string())
            .collect())
    }

    /// Poll the mock-mode wire-command log until `pred` accepts it,
    /// tolerating transient fetch failures throughout, and return the
    /// matching log.
    ///
    /// Retrying the fetch — not just the assertion — is what keeps a
    /// scenario alive on a loaded runner. A single loopback request can
    /// blow its deadline while the driver behaviour under test is
    /// perfectly fine, so one hiccup must not fail an assertion that the
    /// next poll would have satisfied. See `docs/skills/testing.md` §5.9.
    pub async fn wait_for_command_log(
        &self,
        timeout: Duration,
        pred: impl Fn(&[String]) -> bool,
    ) -> Result<Vec<String>, CommandLogTimeout> {
        // Recomputed on every poll, not captured once: a reload can
        // land mid-wait, and the boundary has to move with it.
        self.wait_for_command_log_including_startup(timeout, move |log| {
            pred(log.get(end_of_last_startup(log)..).unwrap_or(&[]))
        })
        .await
        .map(|log| {
            let from = end_of_last_startup(&log);
            log.into_iter().skip(from).collect()
        })
        .map_err(|e| match e {
            CommandLogTimeout::NoMatch(log) => {
                let from = end_of_last_startup(&log);
                CommandLogTimeout::NoMatch(log.into_iter().skip(from).collect())
            }
            other @ CommandLogTimeout::FetchFailed(_) => other,
        })
    }

    /// As [`Self::wait_for_command_log`], but `pred` sees the startup
    /// frames too. Only the steps whose subject is the startup want
    /// this.
    pub async fn wait_for_command_log_including_startup(
        &self,
        timeout: Duration,
        pred: impl Fn(&[String]) -> bool,
    ) -> Result<Vec<String>, CommandLogTimeout> {
        let deadline = Instant::now() + timeout;
        let mut last_log: Option<Vec<String>> = None;
        let mut last_err = "no request completed".to_string();
        loop {
            // Clamp each attempt to what is left of the caller's window.
            // `timeout` is the bound a step promises — `within {secs}
            // seconds` in a feature file — and a fetch that stalls on the
            // client's own budget would otherwise outlive it and let a
            // late frame satisfy an assertion that should have failed.
            let remaining = deadline.saturating_duration_since(Instant::now());
            match tokio::time::timeout(remaining, self.try_command_log()).await {
                Ok(Ok(log)) => {
                    if pred(&log) {
                        return Ok(log);
                    }
                    last_log = Some(log);
                }
                Ok(Err(e)) => last_err = e,
                Err(_) => last_err = format!("no response within the window's last {remaining:?}"),
            }
            if Instant::now() >= deadline {
                break;
            }
            sleep(DEBUG_POLL_INTERVAL).await;
        }
        Err(match last_log {
            Some(log) => CommandLogTimeout::NoMatch(log),
            None => CommandLogTimeout::FetchFailed(last_err),
        })
    }

    /// Fetch the mock-mode wire-command log from the end of the most
    /// recent startup, retrying transient fetch failures. Fails the
    /// scenario only if no read ever succeeds.
    ///
    /// The startup frames are dropped on purpose. A cold
    /// `SharedTransport::start` asserts the no-client safety state, so
    /// `:L1`, `:L2` and `:K1` are on the wire before any scenario does
    /// anything — and an assertion that a disconnect, an abort or a
    /// tracking-off issued one of them would be satisfied by the boot
    /// alone.
    ///
    /// *Most recent*, not first, and that is the whole point of finding
    /// the boundary in the log rather than counting frames at
    /// `start_service`. A `config.apply` reload is a second startup on
    /// the same mock mount: it runs the shutdown halt, then another
    /// handshake, then another `:L1`/`:L2`/`:K1`. A fixed offset leaves
    /// all of that sitting in what a scenario reads as its own traffic,
    /// so a scenario that reloads and then asserts `:K1` would pass
    /// without doing anything — the exact masking this helper exists to
    /// prevent, moved rather than removed. Cutting at the last startup
    /// drops the reload's handshake and halt, and the shutdown halt
    /// ahead of them, because all of it precedes that boundary.
    ///
    /// A scenario whose subject *is* a startup reads
    /// [`Self::command_log_including_startup`] instead.
    pub async fn command_log(&self) -> Vec<String> {
        let log = self.command_log_including_startup().await;
        let from = end_of_last_startup(&log);
        log.into_iter().skip(from).collect()
    }

    /// The whole log, startup included. For the steps whose subject
    /// *is* the startup — the handshake order, the parameter cache it
    /// seeds, the single `:F1` that proves one open.
    pub async fn command_log_including_startup(&self) -> Vec<String> {
        match self
            .wait_for_command_log_including_startup(DEBUG_RETRY_WINDOW, |_| true)
            .await
        {
            Ok(log) => log,
            Err(CommandLogTimeout::FetchFailed(e)) => panic!(
                "could not read mock-commands within {DEBUG_RETRY_WINDOW:?}; last failure: {e}"
            ),
            Err(CommandLogTimeout::NoMatch(_)) => unreachable!("the predicate accepts any log"),
        }
    }

    pub fn clear_error(&mut self) {
        self.last_error = None;
        self.last_error_code = None;
    }

    pub fn record_error(&mut self, e: ASCOMError) {
        self.last_error_code = Some(e.code.raw());
        self.last_error = Some(e.message.to_string());
    }

    pub fn record_destination_pier_side(&mut self, side: PierSide) {
        self.last_destination_pier_side = Some(side);
        self.clear_error();
    }

    /// Re-read the config file the running service was started with and
    /// parse it back into a [`Config`]. Used by `SetPark` scenarios to
    /// assert the file was rewritten in-place.
    pub fn read_persisted_config(&self) -> Config {
        let dir = self
            .temp_dir
            .as_ref()
            .expect("temp_dir not initialised — call start_service first");
        let path = dir.path().join("config.json");
        let content = std::fs::read_to_string(&path).expect("read persisted config");
        serde_json::from_str(&content).expect("parse persisted config")
    }
}

/// Reasonable defaults for BDD scenarios: USB transport with a mock
/// path (the `mock` feature replaces the factory anyway), discovery
/// disabled, server bound to port 0 so each test gets an ephemeral
/// port, and a tight `settle_after_slew` so the slew-completion
/// watcher resolves quickly.
fn default_test_config() -> Config {
    Config {
        transport: TransportConfig::Usb(UsbConfig {
            port: "/dev/mock".to_string(),
            baud_rate: 115_200,
            command_timeout: Duration::from_secs(2),
            polling_interval: Duration::from_millis(50),
        }),
        server: AlpacaServerConfig::new(0),
        mount: MountConfig {
            settle_after_slew: Duration::from_millis(0),
            // BDD scenarios pass hardcoded RA / Dec targets (the
            // canonical example is `RA = 6.0 h, Dec = 30°`) whose
            // computed mech-HA depends on wallclock LST. Disable the
            // binding-zone safety gate (`CwExclusionZone::Disabled`,
            // JSON `null`) so those scenarios don't intermittently trip
            // `INVALID_VALUE` when the test happens to run at an
            // LST that puts the target inside the default
            // `(0.95, 11.05)` zone. The gate itself is exercised by
            // the unit tests in
            // `mount_device::tests::slew_async_refuses_ra_target_in_binding_zone`.
            cw_exclusion_zone: CwExclusionZone::Disabled,
            // Same wallclock-LST reasoning for the altitude floor: a
            // hardcoded RA/Dec target's apparent altitude depends on
            // when the test runs. `-90°` never rejects. The floor
            // itself is exercised by altitude_floor.feature, whose
            // steps address targets by hour angle and configure the
            // floor explicitly.
            min_altitude_degrees: MinAltitudeDegrees::try_new(-90.0).expect("-90 is a valid floor"),
            // Frame-neutral test baseline, pinned explicitly so the
            // scenarios' hardcoded RA/Dec/tick expectations never
            // depend on the ship default (a named pose would seed the
            // firmware encoder on every fresh-power-up connect and
            // shift the frame). Scenarios about seeding and
            // anchored-frame parking opt in with the
            // `configured with unpark_from_ap_position` Given.
            unpark_from_ap_position: ApPark::ApPark0,
            ..MountConfig::default()
        },
    }
}

/// Poll the Alpaca client until a Telescope device appears. The service
/// announces itself to mDNS / discovery once the binary's listener is
/// up; we keep retrying until then.
async fn acquire_mount(handle: &ServiceHandle) -> Arc<dyn Telescope> {
    let addr = SocketAddr::from(([127, 0, 0, 1], handle.port));
    let client = AlpacaClient::new_from_addr(addr);
    for _ in 0..60 {
        sleep(Duration::from_millis(500)).await;
        if let Ok(mut devices) = client.get_devices().await {
            if let Some(TypedDevice::Telescope(mount)) = devices.next() {
                return mount;
            }
        }
    }
    panic!("star-adventurer-gti did not become healthy within 30 seconds");
}

/// Read `mount.description` from `config.get` via a fresh client, returning
/// `None` on any transport/parse failure (e.g. mid-reload).
async fn try_get_mount_description(addr: SocketAddr) -> Option<String> {
    let client = AlpacaClient::new_from_addr(addr);
    let mut devices = client.get_devices().await.ok()?;
    if let Some(TypedDevice::Telescope(mount)) = devices.next() {
        let body = mount
            .action("config.get".to_string(), String::new())
            .await
            .ok()?;
        let parsed: Value = serde_json::from_str(&body).ok()?;
        return parsed["config"]["mount"]["description"]
            .as_str()
            .map(str::to_string);
    }
    None
}
