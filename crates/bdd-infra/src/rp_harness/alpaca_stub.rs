//! Restartable in-process stub of an ASCOM Alpaca device service.
//!
//! rp's session-recovery BDD scenarios (rp.md § Device Session Recovery)
//! need a downstream Alpaca service they can stop and bring back **on
//! the same port** with its server-side state gone — exactly what a
//! real device-service restart does, and something the shared `OmniSim`
//! instance must never do mid-run. The stub serves exactly one device
//! at device number 0 (a `SafetyMonitor`, a `Camera`, or a `Focuser`)
//! with the wire shape rp's Alpaca client speaks: reads answer through
//! the Alpaca `{Value, ErrorNumber, ErrorMessage}` envelope, and any
//! device read issued before `Connected = true` answers ASCOM
//! `NOT_CONNECTED` (0x407) the way a real driver does.
//!
//! The focuser variant exists for rp's Focuser Temperature Watch
//! scenarios (rp.md § Focuser Temperature Watch): its `Temperature`
//! reading is scripted per scenario — a value, `NOT_IMPLEMENTED`, or a
//! fault — and every reading it serves is counted, so a scenario can
//! wait on "the watch has polled again" instead of sleeping.

use std::net::SocketAddr;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::{Arc, RwLock};
use std::time::Duration;

use axum::routing::get;
use axum::{Json, Router};
use serde_json::json;

/// ASCOM `NOT_CONNECTED` error number (0x407), answered by device reads
/// while the stub's server-side `Connected` state is false.
const NOT_CONNECTED_ERROR_NUMBER: u32 = 0x407;
/// ASCOM `NOT_IMPLEMENTED` error number (0x400), answered by the
/// focuser's `Temperature` read when its probe is scripted absent.
const NOT_IMPLEMENTED_ERROR_NUMBER: u32 = 0x400;
/// ASCOM `UNSPECIFIED_ERROR` error number (0x500), answered by the
/// focuser's `Temperature` read when its probe is scripted faulty.
const UNSPECIFIED_ERROR_NUMBER: u32 = 0x500;

/// Canned invariant sensor metadata served by the [`StubDevice::Camera`]
/// variant once connected, mirroring what rp's connect routine caches.
pub const STUB_CAMERA_MAX_ADU: u32 = 65_535;
/// Pixel pitch in microns for both axes.
pub const STUB_CAMERA_PIXEL_SIZE_UM: f64 = 3.76;
/// Sensor width in pixels.
pub const STUB_CAMERA_WIDTH_PX: u32 = 1920;
/// Sensor height in pixels.
pub const STUB_CAMERA_HEIGHT_PX: u32 = 1080;

/// Which single device the stub hosts at device number 0.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum StubDevice {
    SafetyMonitor,
    Camera,
    Focuser,
}

impl StubDevice {
    const fn type_name(self) -> &'static str {
        match self {
            Self::SafetyMonitor => "SafetyMonitor",
            Self::Camera => "Camera",
            Self::Focuser => "Focuser",
        }
    }

    const fn api_path(self) -> &'static str {
        match self {
            Self::SafetyMonitor => "safetymonitor",
            Self::Camera => "camera",
            Self::Focuser => "focuser",
        }
    }
}

/// What the focuser variant's `Temperature` property answers while
/// connected. Models the probe, not the process: it carries over a
/// [`AlpacaDeviceStub::restart`] like the safety monitor's reading.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum FocuserProbe {
    /// A reading in °C.
    Reading(f64),
    /// No probe: ASCOM `NOT_IMPLEMENTED` (0x400), what a focuser
    /// without a temperature sensor answers.
    NotImplemented,
    /// A wired probe whose read fails: ASCOM `UNSPECIFIED_ERROR`
    /// (0x500).
    Fault,
}

/// Server-side state of one stub incarnation. A [`AlpacaDeviceStub::restart`]
/// replaces it wholesale, which is the point: the fresh incarnation has
/// `connected = false`, like a freshly restarted device service.
#[derive(Debug)]
struct StubState {
    connected: AtomicBool,
    is_safe: AtomicBool,
    probe: RwLock<FocuserProbe>,
    /// `Temperature` reads served while connected by this incarnation.
    temperature_reads: AtomicU32,
}

/// In-process Alpaca device service that can be stopped and brought
/// back on the same port. Hold the handle alive for the scenario;
/// dropping it shuts the listener down best-effort.
#[derive(Debug)]
pub struct AlpacaDeviceStub {
    port: u16,
    device: StubDevice,
    state: Arc<StubState>,
    shutdown_tx: Option<tokio::sync::oneshot::Sender<()>>,
    task: Option<tokio::task::JoinHandle<()>>,
}

impl AlpacaDeviceStub {
    /// Spawn the stub on an OS-assigned loopback port, with the device
    /// disconnected (as a freshly started service would be) and — for
    /// the safety-monitor variant — reporting safe once connected.
    ///
    /// # Panics
    ///
    /// Panics if no loopback port can be bound.
    #[must_use]
    pub fn start(device: StubDevice) -> Self {
        let listener = bind_reuse(0).expect("failed to bind Alpaca stub");
        let port = listener
            .local_addr()
            .expect("stub has no local addr")
            .port();
        let mut stub = Self {
            port,
            device,
            state: fresh_state(true, FocuserProbe::NotImplemented),
            shutdown_tx: None,
            task: None,
        };
        stub.spawn(listener);
        stub
    }

    /// Allocate a port and return with the service **not** running —
    /// the "device service was down when rp started" opening position.
    /// Bring it up later with [`Self::restart`].
    ///
    /// # Panics
    ///
    /// Panics if no loopback port can be bound.
    pub async fn start_stopped(device: StubDevice) -> Self {
        let mut stub = Self::start(device);
        stub.stop().await;
        stub
    }

    /// The base URL rp's `alpaca_url` config should point at. Stable
    /// across [`Self::stop`] / [`Self::restart`].
    #[must_use]
    pub fn url(&self) -> String {
        format!("http://127.0.0.1:{}", self.port)
    }

    /// Stop the service and wait until the listener is fully released,
    /// so a later [`Self::restart`] can rebind the same port.
    pub async fn stop(&mut self) {
        if let Some(tx) = self.shutdown_tx.take() {
            let _ = tx.send(());
        }
        if let Some(task) = self.task.take() {
            let _ = task.await;
        }
    }

    /// Bring the service back on the same port with fresh server-side
    /// state — `Connected` is false again, exactly like a restarted
    /// device service. The configured `is_safe` reading and the
    /// focuser probe carry over (they model the weather and the
    /// sensor, not the process); the read counter starts from zero.
    ///
    /// # Panics
    ///
    /// Panics if the original port cannot be rebound within 10 s.
    pub async fn restart(&mut self) {
        self.stop().await;
        self.state = fresh_state(
            self.state.is_safe.load(Ordering::SeqCst),
            self.focuser_probe(),
        );
        // 100 × 100 ms = a 10 s rebind budget, generous for the rare
        // lingering-socket case since `stop` already joined the server.
        let mut bound = bind_reuse(self.port);
        for _ in 0..100u32 {
            if bound.is_ok() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
            bound = bind_reuse(self.port);
        }
        let listener = bound.expect("could not rebind the Alpaca stub port within 10s");
        self.spawn(listener);
    }

    /// Set the reading the safety-monitor variant reports while
    /// connected.
    pub fn set_is_safe(&self, is_safe: bool) {
        self.state.is_safe.store(is_safe, Ordering::SeqCst);
    }

    /// Whether a client has issued `Connected = true` to the current
    /// incarnation.
    #[must_use]
    pub fn is_connected(&self) -> bool {
        self.state.connected.load(Ordering::SeqCst)
    }

    /// Script what the focuser variant's `Temperature` read answers
    /// from now on. Takes effect on the next read, stopped or running.
    ///
    /// # Panics
    ///
    /// Panics if the probe lock was poisoned by a panicking request
    /// handler — a stub bug, not a scenario condition.
    pub fn set_focuser_probe(&self, probe: FocuserProbe) {
        *self.state.probe.write().expect("stub probe lock poisoned") = probe;
    }

    /// The focuser variant's scripted probe.
    ///
    /// # Panics
    ///
    /// Panics if the probe lock was poisoned (see
    /// [`Self::set_focuser_probe`]).
    #[must_use]
    pub fn focuser_probe(&self) -> FocuserProbe {
        *self.state.probe.read().expect("stub probe lock poisoned")
    }

    /// How many `Temperature` reads the current incarnation has served
    /// while connected — reads refused as `NOT_CONNECTED` do not count.
    /// Resets to zero on [`Self::restart`]. A scenario waits on this
    /// to know rp's temperature watch has polled again.
    #[must_use]
    pub fn focuser_probe_reads(&self) -> u32 {
        self.state.temperature_reads.load(Ordering::SeqCst)
    }

    fn spawn(&mut self, listener: tokio::net::TcpListener) {
        let app = router(self.device, self.state.clone());
        let (shutdown_tx, shutdown_rx) = tokio::sync::oneshot::channel::<()>();
        let task = tokio::spawn(async move {
            axum::serve(listener, app)
                .with_graceful_shutdown(async {
                    let _ = shutdown_rx.await;
                })
                .await
                .expect("Alpaca device stub failed");
        });
        self.shutdown_tx = Some(shutdown_tx);
        self.task = Some(task);
    }
}

impl Drop for AlpacaDeviceStub {
    fn drop(&mut self) {
        if let Some(tx) = self.shutdown_tx.take() {
            let _ = tx.send(());
        }
    }
}

fn fresh_state(is_safe: bool, probe: FocuserProbe) -> Arc<StubState> {
    Arc::new(StubState {
        connected: AtomicBool::new(false),
        is_safe: AtomicBool::new(is_safe),
        probe: RwLock::new(probe),
        temperature_reads: AtomicU32::new(0),
    })
}

/// Bind a loopback listener with `SO_REUSEADDR`, so a restart can
/// reclaim the port even while old connections linger in `TIME_WAIT`.
fn bind_reuse(port: u16) -> std::io::Result<tokio::net::TcpListener> {
    let socket = tokio::net::TcpSocket::new_v4()?;
    socket.set_reuseaddr(true)?;
    socket.bind(SocketAddr::from(([127, 0, 0, 1], port)))?;
    socket.listen(64)
}

fn router(device: StubDevice, state: Arc<StubState>) -> Router {
    let connected_path = format!("/api/v1/{}/0/connected", device.api_path());

    let devices_body = json!({
        "Value": [{
            "DeviceName": "Recovery Stub",
            "DeviceType": device.type_name(),
            "DeviceNumber": 0,
            "UniqueID": "bdd-alpaca-recovery-stub-0"
        }],
        "ErrorNumber": 0,
        "ErrorMessage": ""
    });

    let get_connected_state = state.clone();
    let put_connected_state = state.clone();
    let app = Router::new()
        .route(
            "/management/v1/configureddevices",
            get(move || {
                let body = devices_body.clone();
                async move { Json(body) }
            }),
        )
        .route(
            &connected_path,
            // `Connected` itself is readable while disconnected — it is
            // how a client finds out.
            get(move || {
                let state = get_connected_state.clone();
                async move { value_response(&json!(state.connected.load(Ordering::SeqCst))) }
            })
            .put(move |form: axum::Form<Vec<(String, String)>>| {
                let state = put_connected_state.clone();
                async move {
                    let requested = form
                        .0
                        .iter()
                        .find(|(k, _)| k == "Connected")
                        .is_some_and(|(_, v)| v.eq_ignore_ascii_case("true"));
                    state.connected.store(requested, Ordering::SeqCst);
                    Json(json!({ "ErrorNumber": 0, "ErrorMessage": "" }))
                }
            }),
        );

    match device {
        StubDevice::SafetyMonitor => safety_monitor_routes(app, state),
        StubDevice::Camera => camera_routes(app, &state),
        StubDevice::Focuser => focuser_routes(app, state),
    }
}

/// `IsSafe`, gated on `Connected`.
fn safety_monitor_routes(app: Router, state: Arc<StubState>) -> Router {
    app.route(
        "/api/v1/safetymonitor/0/issafe",
        get(move || {
            let state = state.clone();
            async move {
                if state.connected.load(Ordering::SeqCst) {
                    value_response(&json!(state.is_safe.load(Ordering::SeqCst)))
                } else {
                    not_connected_response()
                }
            }
        }),
    )
}

/// The connect-time property cache rp reads, gated on `Connected`.
fn camera_routes(app: Router, state: &Arc<StubState>) -> Router {
    let mut with_metadata = app;
    for (path, value) in [
        ("/api/v1/camera/0/maxadu", json!(STUB_CAMERA_MAX_ADU)),
        (
            "/api/v1/camera/0/pixelsizex",
            json!(STUB_CAMERA_PIXEL_SIZE_UM),
        ),
        (
            "/api/v1/camera/0/pixelsizey",
            json!(STUB_CAMERA_PIXEL_SIZE_UM),
        ),
        ("/api/v1/camera/0/cameraxsize", json!(STUB_CAMERA_WIDTH_PX)),
        ("/api/v1/camera/0/cameraysize", json!(STUB_CAMERA_HEIGHT_PX)),
    ] {
        let route_state = state.clone();
        with_metadata = with_metadata.route(
            path,
            get(move || {
                let state = route_state.clone();
                async move {
                    if state.connected.load(Ordering::SeqCst) {
                        value_response(&value)
                    } else {
                        not_connected_response()
                    }
                }
            }),
        );
    }
    with_metadata
}

/// `Temperature`, gated on `Connected`, answering the scripted probe
/// and counting every read it serves.
fn focuser_routes(app: Router, state: Arc<StubState>) -> Router {
    app.route(
        "/api/v1/focuser/0/temperature",
        get(move || {
            let state = state.clone();
            async move {
                if !state.connected.load(Ordering::SeqCst) {
                    return not_connected_response();
                }
                state.temperature_reads.fetch_add(1, Ordering::SeqCst);
                let probe = *state.probe.read().expect("stub probe lock poisoned");
                match probe {
                    FocuserProbe::Reading(value) => value_response(&json!(value)),
                    FocuserProbe::NotImplemented => error_response(
                        NOT_IMPLEMENTED_ERROR_NUMBER,
                        "NOT_IMPLEMENTED: Temperature is not implemented",
                    ),
                    FocuserProbe::Fault => error_response(
                        UNSPECIFIED_ERROR_NUMBER,
                        "UNSPECIFIED_ERROR: the temperature probe did not answer",
                    ),
                }
            }
        }),
    )
}

fn value_response(value: &serde_json::Value) -> Json<serde_json::Value> {
    Json(json!({ "Value": value, "ErrorNumber": 0, "ErrorMessage": "" }))
}

fn not_connected_response() -> Json<serde_json::Value> {
    error_response(
        NOT_CONNECTED_ERROR_NUMBER,
        "NOT_CONNECTED: the device is not connected",
    )
}

fn error_response(number: u32, message: &str) -> Json<serde_json::Value> {
    Json(json!({ "ErrorNumber": number, "ErrorMessage": message }))
}

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    async fn get_json(url: &str) -> serde_json::Value {
        reqwest::Client::new()
            .get(url)
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap()
    }

    async fn put_connected(base: &str, path: &str, connected: bool) {
        let resp: serde_json::Value = reqwest::Client::new()
            .put(format!("{base}{path}"))
            .form(&[("Connected", if connected { "True" } else { "False" })])
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        assert_eq!(resp["ErrorNumber"], 0, "PUT connected failed: {resp}");
    }

    #[tokio::test]
    async fn safety_monitor_reads_not_connected_until_connected() {
        let stub = AlpacaDeviceStub::start(StubDevice::SafetyMonitor);
        let base = stub.url();

        let devices = get_json(&format!("{base}/management/v1/configureddevices")).await;
        assert_eq!(devices["Value"][0]["DeviceType"], "SafetyMonitor");

        let before = get_json(&format!("{base}/api/v1/safetymonitor/0/issafe")).await;
        assert_eq!(
            before["ErrorNumber"], NOT_CONNECTED_ERROR_NUMBER,
            "a disconnected monitor must answer NOT_CONNECTED, got: {before}"
        );

        put_connected(&base, "/api/v1/safetymonitor/0/connected", true).await;
        let after = get_json(&format!("{base}/api/v1/safetymonitor/0/issafe")).await;
        assert_eq!(
            after["Value"], true,
            "connected monitor reads safe: {after}"
        );

        stub.set_is_safe(false);
        let unsafe_reading = get_json(&format!("{base}/api/v1/safetymonitor/0/issafe")).await;
        assert_eq!(
            unsafe_reading["Value"], false,
            "set_is_safe must flip the reading: {unsafe_reading}"
        );
    }

    /// A restart retries the rebind while something else briefly holds
    /// the port — the freed-late case the retry loop exists for.
    #[tokio::test]
    async fn restart_waits_out_a_briefly_occupied_port() {
        let mut stub = AlpacaDeviceStub::start(StubDevice::SafetyMonitor);
        let base = stub.url();
        stub.stop().await;

        let addr = base.trim_start_matches("http://").to_owned();
        let blocker = std::net::TcpListener::bind(&addr).expect("blocker must grab the freed port");
        let release = tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(300)).await;
            drop(blocker);
        });

        stub.restart().await;
        release.await.unwrap();
        let connected = get_json(&format!("{base}/api/v1/safetymonitor/0/connected")).await;
        assert_eq!(
            connected["Value"], false,
            "the restarted stub must serve on the original port: {connected}"
        );
    }

    #[tokio::test]
    async fn restart_rebinds_the_same_port_and_forgets_connected() {
        let mut stub = AlpacaDeviceStub::start(StubDevice::SafetyMonitor);
        let base = stub.url();
        put_connected(&base, "/api/v1/safetymonitor/0/connected", true).await;
        assert!(stub.is_connected());

        stub.restart().await;
        assert_eq!(stub.url(), base, "restart must keep the port");
        let connected = get_json(&format!("{base}/api/v1/safetymonitor/0/connected")).await;
        assert_eq!(
            connected["Value"], false,
            "a restarted service forgets Connected: {connected}"
        );
    }

    #[tokio::test]
    async fn stopped_stub_refuses_connections() {
        let mut stub = AlpacaDeviceStub::start(StubDevice::Camera);
        let base = stub.url();
        stub.stop().await;
        let err = reqwest::Client::new()
            .get(format!("{base}/api/v1/camera/0/connected"))
            .send()
            .await;
        assert!(err.is_err(), "a stopped stub must refuse connections");
    }

    #[tokio::test]
    async fn camera_metadata_gated_on_connected() {
        let stub = AlpacaDeviceStub::start(StubDevice::Camera);
        let base = stub.url();
        let before = get_json(&format!("{base}/api/v1/camera/0/maxadu")).await;
        assert_eq!(before["ErrorNumber"], NOT_CONNECTED_ERROR_NUMBER);

        put_connected(&base, "/api/v1/camera/0/connected", true).await;
        let after = get_json(&format!("{base}/api/v1/camera/0/maxadu")).await;
        assert_eq!(after["Value"], STUB_CAMERA_MAX_ADU);
    }

    /// A disconnected focuser answers `NOT_CONNECTED` without counting
    /// the read; connected, each scripted probe state maps to its wire
    /// answer and every served read is counted.
    #[tokio::test]
    async fn focuser_temperature_follows_the_scripted_probe_and_counts_reads() {
        let stub = AlpacaDeviceStub::start(StubDevice::Focuser);
        let base = stub.url();
        let temperature = format!("{base}/api/v1/focuser/0/temperature");

        let devices = get_json(&format!("{base}/management/v1/configureddevices")).await;
        assert_eq!(devices["Value"][0]["DeviceType"], "Focuser");

        let before = get_json(&temperature).await;
        assert_eq!(before["ErrorNumber"], NOT_CONNECTED_ERROR_NUMBER);
        assert_eq!(
            stub.focuser_probe_reads(),
            0,
            "a refused read is not counted"
        );

        put_connected(&base, "/api/v1/focuser/0/connected", true).await;
        let absent = get_json(&temperature).await;
        assert_eq!(absent["ErrorNumber"], NOT_IMPLEMENTED_ERROR_NUMBER);

        stub.set_focuser_probe(FocuserProbe::Reading(10.5));
        let reading = get_json(&temperature).await;
        assert_eq!(reading["Value"], 10.5);

        stub.set_focuser_probe(FocuserProbe::Fault);
        let fault = get_json(&temperature).await;
        assert_eq!(fault["ErrorNumber"], UNSPECIFIED_ERROR_NUMBER);

        assert_eq!(stub.focuser_probe_reads(), 3);
    }

    /// The probe models the sensor, so it survives a restart; the read
    /// counter belongs to the incarnation, so it does not.
    #[tokio::test]
    async fn focuser_restart_keeps_the_probe_and_resets_the_read_count() {
        let mut stub = AlpacaDeviceStub::start(StubDevice::Focuser);
        let base = stub.url();
        stub.set_focuser_probe(FocuserProbe::Reading(4.0));
        put_connected(&base, "/api/v1/focuser/0/connected", true).await;
        let _ = get_json(&format!("{base}/api/v1/focuser/0/temperature")).await;
        assert_eq!(stub.focuser_probe_reads(), 1);

        stub.restart().await;
        assert_eq!(stub.focuser_probe(), FocuserProbe::Reading(4.0));
        assert_eq!(stub.focuser_probe_reads(), 0);
        put_connected(&base, "/api/v1/focuser/0/connected", true).await;
        let reading = get_json(&format!("{base}/api/v1/focuser/0/temperature")).await;
        assert_eq!(reading["Value"], 4.0);
    }
}
