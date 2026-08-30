//! Cucumber `World` for the planetarium-bridge BDD suite.
//!
//! Each scenario spawns the planetarium-bridge binary and drives it through
//! the typed `ascom-alpaca` Telescope client over real HTTP, with a stub rp
//! MCP server (`stub_rp.rs`) standing in for rp — the harness the design
//! doc's Testing section specifies (docs/services/planetarium-bridge.md).

use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

use ascom_alpaca::api::{Telescope, TypedDevice};
use ascom_alpaca::ASCOMErrorCode;
use ascom_alpaca::Client as AlpacaClient;
use bdd_infra::doctor_smoke::{DoctorSmokeState, DoctorSmokeWorld};
use bdd_infra::tls_auth::{TlsAuthSmokeWorld, TlsAuthState};
use bdd_infra::ServiceHandle;
use cucumber::World;
use tempfile::TempDir;

use crate::stub_rp::StubRp;

/// How long any wait in this suite may run before it reports a hang.
///
/// A budget here bounds a hang; it is not a timing assertion. Nothing in
/// this suite asserts how *quickly* the bridge does anything — the
/// scenarios that care about duration declare a simulated slew duration and
/// then check where the mount ended up — so sizing a budget as a small
/// multiple of the expected wait reads like a contract while enforcing
/// none, and only decides how long the suite waits before calling the
/// bridge stuck. Sizing it that way is what made the tightest of them the
/// first to fail on the coverage leg, where instrumented binaries and the
/// whole target graph in parallel stretch every poll's round trip. So: one
/// generous budget for every wait, on the same scale as the allowance the
/// harness already gives a service just to print its bound port.
///
/// Cucumber runs the scenarios concurrently, so a systemic hang costs the
/// suite this once rather than once per scenario — well inside the
/// `size = "large"` target it runs under.
pub const WAIT_BUDGET: Duration = Duration::from_secs(30);

/// Gap between polls inside a wait — short enough that the elapsed time a
/// timed-out wait reports is about the event and not about the sleep.
pub const POLL_INTERVAL: Duration = Duration::from_millis(100);

/// The scenario's `report_altitude_floor_deg` config value.
#[derive(Debug, Default, Clone, Copy, PartialEq)]
pub enum FloorSetting {
    /// Key omitted — the service default (10.0) applies.
    #[default]
    ServiceDefault,
    /// An explicit floor in degrees.
    Degrees(f64),
    /// Explicit `null` — the reported-position policy is disabled.
    Null,
}

#[derive(Debug, Default, World)]
pub struct BridgeWorld {
    pub handle: Option<ServiceHandle>,
    pub telescope: Option<Arc<dyn Telescope>>,
    pub temp_dir: Option<TempDir>,
    pub stub_rp: Option<StubRp>,
    /// Staged config and captured output of the shared doctor smoke — it
    /// runs the binary's `doctor` subcommand, never a server.
    pub doctor_smoke: DoctorSmokeState,
    /// Throwaway PKI and credentials for the shared TLS + auth smoke.
    pub tls_auth: TlsAuthState,

    // Config knobs set by Given steps before the service starts.
    pub floor: FloorSetting,
    pub assume_epoch: Option<String>,
    pub slew_duration: Option<String>,
    pub spool_max_entries: Option<u64>,
    pub replay_backoff_max: Option<String>,
    pub site_latitude_deg: Option<f64>,
    pub site_longitude_deg: Option<f64>,
    /// rp MCP URL for the bridge config; set by the stub-rp /
    /// rp-unreachable Givens. When still `None` at start, an unreachable
    /// reserved-band URL is used, so scenarios that never look at imports
    /// need no stub (their Aligns spool into the scenario temp dir).
    pub rp_url: Option<String>,

    /// Config file path of the running bridge — reused verbatim by the
    /// restart step so the spool location survives.
    pub config_path: Option<String>,

    // Result stashes ("When does, Then asserts").
    pub last_error_code: Option<u16>,
    /// Coordinates of the last zenith Align (`ra_hours`, `dec_degrees`).
    pub last_synced: Option<(f64, f64)>,
}

impl BridgeWorld {
    fn temp_dir(&mut self) -> &TempDir {
        self.temp_dir
            .get_or_insert_with(|| TempDir::new().expect("temp dir"))
    }

    /// The scenario's spool file location (inside the scenario temp dir, so
    /// it survives a bridge restart but never a scenario boundary).
    pub fn spool_path(&mut self) -> PathBuf {
        self.temp_dir().path().join("spool.jsonl")
    }

    /// The port of the configured rp URL — where the "rp comes back up"
    /// step must re-bind the stub.
    pub fn configured_rp_port(&self) -> u16 {
        let url = self.rp_url.as_ref().expect("no rp URL configured");
        url.strip_prefix("http://127.0.0.1:")
            .and_then(|rest| rest.split('/').next())
            .and_then(|port| port.parse().ok())
            .expect("rp URL is not the harness shape http://127.0.0.1:PORT/mcp")
    }

    /// The service-specific half of a bridge config — everything but the
    /// `server` block, which the doctor and TLS/auth smokes each fill in
    /// their own way. Spelling out every block puts the whole typed load
    /// path (site newtypes, the `assume_epoch` enum, humantime durations)
    /// under the doctor smoke's clean-report scenario.
    ///
    /// `spool.path` is deliberately absent: only a `&mut self` path can mint
    /// a scenario temp dir, so `start_with_tls_auth` inserts it before
    /// spawning. Doctor needs none — it parses the file and never resolves
    /// the spool location.
    fn smoke_base_config() -> serde_json::Value {
        serde_json::json!({
            "site": {
                "site_latitude_deg": 33.0,
                "site_longitude_deg": -117.0,
                "site_elevation_m": 0.0,
            },
            // Neither smoke imports anything, and a dead URL keeps the
            // import worker off whatever is listening on rp's real port.
            "rp": { "mcp_server_url": unreachable_rp_url() },
            "device": {
                "slew_duration": "3s",
                "assume_epoch": "j2000",
                "report_altitude_floor_deg": 10.0,
            },
            "spool": { "max_entries": 1000, "replay_backoff_max": "60s" },
        })
    }

    fn write_config(&mut self) -> String {
        let rp_url = self.rp_url.get_or_insert_with(unreachable_rp_url).clone();
        let spool_path = self.spool_path();

        let mut device = serde_json::Map::new();
        match self.floor {
            FloorSetting::ServiceDefault => {}
            FloorSetting::Degrees(v) => {
                device.insert("report_altitude_floor_deg".into(), v.into());
            }
            FloorSetting::Null => {
                device.insert("report_altitude_floor_deg".into(), serde_json::Value::Null);
            }
        }
        if let Some(epoch) = &self.assume_epoch {
            device.insert("assume_epoch".into(), epoch.clone().into());
        }
        if let Some(duration) = &self.slew_duration {
            device.insert("slew_duration".into(), duration.clone().into());
        }

        let mut spool = serde_json::Map::new();
        spool.insert(
            "path".into(),
            spool_path.to_str().expect("utf8 spool path").into(),
        );
        if let Some(max) = self.spool_max_entries {
            spool.insert("max_entries".into(), max.into());
        }
        if let Some(backoff) = &self.replay_backoff_max {
            spool.insert("replay_backoff_max".into(), backoff.clone().into());
        }

        let config = serde_json::json!({
            // Port 0 → OS-assigned; the real port is read from the
            // `bound_addr=` line on stdout by ServiceHandle.
            "server": { "port": 0 },
            "site": {
                "site_latitude_deg": self.site_latitude_deg.unwrap_or(33.0),
                "site_longitude_deg": self.site_longitude_deg.unwrap_or(-117.0),
                "site_elevation_m": 0.0,
            },
            "rp": { "mcp_server_url": rp_url },
            "device": device,
            "spool": spool,
        });

        let path = self.temp_dir().path().join("planetarium-bridge.json");
        std::fs::write(
            &path,
            serde_json::to_string_pretty(&config).expect("serialize config"),
        )
        .expect("write config");
        let path = path.to_str().expect("utf8 config path").to_string();
        self.config_path = Some(path.clone());
        path
    }

    /// Spawn the service binary and acquire the typed Telescope client.
    pub async fn start(&mut self) {
        let config_path = self.write_config();
        let handle = ServiceHandle::start(env!("CARGO_PKG_NAME"), &config_path).await;
        self.handle = Some(handle);
        self.acquire().await;
    }

    /// Stop the bridge process, keeping the scenario config and spool.
    pub async fn stop_bridge(&mut self) {
        self.telescope = None;
        if let Some(handle) = self.handle.as_mut() {
            handle.stop().await;
        }
        self.handle = None;
    }

    /// Start a fresh bridge process on the SAME config file (and therefore
    /// the same spool) — the restart-survival half of the spool contract.
    pub async fn restart(&mut self) {
        assert!(self.handle.is_none(), "stop the bridge before restarting");
        let config_path = self
            .config_path
            .clone()
            .expect("the bridge was never started");
        let handle = ServiceHandle::start(env!("CARGO_PKG_NAME"), &config_path).await;
        self.handle = Some(handle);
        self.acquire().await;
    }

    async fn acquire(&mut self) {
        let port = self.handle.as_ref().expect("service handle").port;
        let addr = SocketAddr::from(([127, 0, 0, 1], port));
        // One client for the whole wait: cloning the crate's shared reqwest
        // client per poll would be correct but pointless churn.
        let client = AlpacaClient::new_from_addr(addr);
        let start = Instant::now();
        while start.elapsed() < WAIT_BUDGET {
            if let Ok(devices) = client.get_devices().await {
                for device in devices {
                    #[allow(clippy::single_match)]
                    match device {
                        TypedDevice::Telescope(telescope) => {
                            self.telescope = Some(telescope);
                            return;
                        }
                        #[allow(unreachable_patterns)]
                        _ => {}
                    }
                }
            }
            tokio::time::sleep(POLL_INTERVAL).await;
        }
        panic!(
            "the bridge registered no Telescope device in {:?} (budget {WAIT_BUDGET:?})",
            start.elapsed()
        );
    }

    pub fn telescope(&self) -> Arc<dyn Telescope> {
        Arc::clone(self.telescope.as_ref().expect("telescope not acquired"))
    }

    pub const fn stub(&self) -> &StubRp {
        self.stub_rp.as_ref().expect("no stub rp running")
    }

    pub async fn connect(&mut self) {
        self.telescope()
            .set_connected(true)
            .await
            .expect("connect planetarium client");
    }

    /// Drive a `SyncToCoordinates` and stash the ASCOM error code (`None` on
    /// success).
    pub async fn try_sync(&mut self, ra_hours: f64, dec_degrees: f64) {
        match self
            .telescope()
            .sync_to_coordinates(ra_hours, dec_degrees)
            .await
        {
            Ok(()) => self.last_error_code = None,
            Err(e) => self.last_error_code = Some(e.code.raw()),
        }
    }

    /// `GET /health` on the Alpaca listener, parsed; `None` while the
    /// endpoint is unreachable or not JSON.
    pub async fn try_health(&self) -> Option<serde_json::Value> {
        let port = self.handle.as_ref().expect("service handle").port;
        reqwest::get(format!("http://127.0.0.1:{port}/health"))
            .await
            .ok()?
            .json()
            .await
            .ok()
    }
}

impl DoctorSmokeWorld for BridgeWorld {
    fn doctor_smoke(&mut self) -> &mut DoctorSmokeState {
        &mut self.doctor_smoke
    }

    /// The smoke base config plus a plain `server` block — the full shape
    /// the bridge's own `deny_unknown_fields` load accepts.
    fn valid_config(&self) -> serde_json::Value {
        let mut config = Self::smoke_base_config();
        config["server"] = serde_json::json!({ "port": 0 });
        config
    }
}

impl TlsAuthSmokeWorld for BridgeWorld {
    fn tls_auth(&mut self) -> &mut TlsAuthState {
        &mut self.tls_auth
    }

    fn base_test_config(&self) -> serde_json::Value {
        Self::smoke_base_config()
    }

    /// Point the spool at the scenario temp dir before spawning: an absent
    /// `spool.path` resolves to — and creates — the *platform* config dir,
    /// which a test must never touch.
    async fn start_with_tls_auth(&mut self, mut config: serde_json::Value) {
        let spool_path = self.spool_path();
        config["spool"]["path"] = spool_path.to_string_lossy().into_owned().into();
        let handle = bdd_infra::tls_auth::spawn_service_handle(
            &mut self.tls_auth,
            env!("CARGO_PKG_NAME"),
            &config,
        )
        .await;
        self.handle = Some(handle);
    }
}

/// Map an ASCOM error-code *name* (as written in the feature files) to its
/// raw `u16`, so Then steps can assert "rejected with ASCOM <NAME>".
pub fn ascom_code(name: &str) -> u16 {
    match name {
        "INVALID_VALUE" => ASCOMErrorCode::INVALID_VALUE.raw(),
        "NOT_CONNECTED" => ASCOMErrorCode::NOT_CONNECTED.raw(),
        "NOT_IMPLEMENTED" => ASCOMErrorCode::NOT_IMPLEMENTED.raw(),
        "INVALID_OPERATION" => ASCOMErrorCode::INVALID_OPERATION.raw(),
        other => panic!("unknown ASCOM error code name: {other}"),
    }
}

/// A URL where nothing listens — the "rp is unreachable" fixture.
///
/// Ports come from a reserved band below the platform ephemeral floor,
/// probed by *connecting* (a refused connect is the same "nobody is there"
/// answer), never by binding and releasing — a bind probe draws from the
/// same range the OS hands to every concurrent `bind(0)` and its listener
/// can leak into forked children (testing.md §5.1, issue #745). Band
/// 23100–24099 stays clear of the rusty-photon service band (111xx) and
/// phd2-guider's reserved band.
pub fn unreachable_rp_url() -> String {
    format!("http://127.0.0.1:{}/mcp", reserved_dead_port())
}

const RESERVED_PORT_BAND_START: u16 = 23100;
const RESERVED_PORT_BAND_LEN: u16 = 1000;
const RESERVED_PORT_TRIES: u16 = 64;

fn reserved_dead_port() -> u16 {
    static START: std::sync::OnceLock<u16> = std::sync::OnceLock::new();
    static CURSOR: std::sync::atomic::AtomicU16 = std::sync::atomic::AtomicU16::new(0);

    // Distinct per process: pids alone are too regular when a harness starts
    // many copies at once.
    let start = *START.get_or_init(|| {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |d| d.subsec_nanos());
        ((std::process::id() ^ nanos) % u32::from(RESERVED_PORT_BAND_LEN)) as u16
    });

    for _ in 0..RESERVED_PORT_TRIES {
        let step = CURSOR.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let candidate =
            RESERVED_PORT_BAND_START + (start.wrapping_add(step) % RESERVED_PORT_BAND_LEN);
        if std::net::TcpStream::connect(("127.0.0.1", candidate)).is_err() {
            return candidate;
        }
    }
    panic!("no free reserved port in {RESERVED_PORT_TRIES} tries");
}
