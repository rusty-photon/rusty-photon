//! The device clients and their poll (docs/services/guider.md § Devices
//! and connections).
//!
//! The guider is a client of one Camera and one Telescope. The poll only
//! reads: every `devices.poll_interval` it reads each server's device list
//! and the device's `Connected`, and on the first read that finds a device
//! connected, and again whenever it comes back, the device's static
//! properties. It never writes `Connected`. `GET /api/v1/devices` and
//! `/health` answer from what it last read.

use std::fmt::Display;
use std::future::Future;
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use ascom_alpaca::api::camera::SensorType;
use ascom_alpaca::api::{Camera, Device, Telescope, TypedDevice};
use ascom_alpaca::Client;
use base64::engine::general_purpose::STANDARD as BASE64;
use base64::Engine;
use tokio::sync::watch;
use tokio_util::sync::CancellationToken;
use tracing::debug;

use crate::config::DeviceAddress;
use crate::error::{GuiderError, Result};

/// How long a call to a device may go without an answer before it has
/// failed (guider.md § Devices and connections, "A call that gets no
/// answer").
pub const CALL_DEADLINE: Duration = Duration::from_secs(10);

/// How long the TCP connect of a call may take: a refused or
/// black-holed host fails here rather than at the deadline.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(5);

/// Why a call to a device failed.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum CallError {
    /// No answer within [`CALL_DEADLINE`].
    #[error("no answer within {}", humantime::format_duration(CALL_DEADLINE))]
    Deadline,
    /// The call answered with an error, or could not be sent.
    #[error("{0}")]
    Failed(String),
}

/// `call` held to [`CALL_DEADLINE`].
async fn call<T, E: Display>(
    call: impl Future<Output = std::result::Result<T, E>>,
) -> std::result::Result<T, CallError> {
    match tokio::time::timeout(CALL_DEADLINE, call).await {
        Ok(Ok(value)) => Ok(value),
        Ok(Err(error)) => Err(CallError::Failed(error.to_string())),
        Err(_) => Err(CallError::Deadline),
    }
}

/// How far a poll got with a device.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Reach {
    /// The device's server does not answer, or a read of the device
    /// failed. A device counts as not answering until its first poll
    /// answers.
    #[default]
    NoAnswer,
    /// The server answers and lists no device of the kind at
    /// `device_number`.
    NotListed,
    /// The device answers and reports `Connected` false.
    NotConnected,
    /// The device answers and reports `Connected` true.
    Connected,
}

impl Reach {
    /// Whether the device answered.
    #[must_use]
    pub const fn reachable(self) -> bool {
        matches!(self, Self::NotConnected | Self::Connected)
    }

    /// Whether the device answered and is connected.
    #[must_use]
    pub const fn connected(self) -> bool {
        matches!(self, Self::Connected)
    }
}

/// What the poll knows of one device.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DeviceState<S> {
    pub reach: Reach,
    /// The `UniqueID` the server listed for the device at the last poll
    /// that read the list, stripped of the whitespace around it.
    pub unique_id: Option<String>,
    /// The static properties last read; `None` until a read succeeds.
    pub statics: Option<S>,
}

impl<S> Default for DeviceState<S> {
    fn default() -> Self {
        Self {
            reach: Reach::NoAnswer,
            unique_id: None,
            statics: None,
        }
    }
}

/// The camera's static properties (guider.md § Alpaca members used).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct CameraSensor {
    pub sensor_type: SensorType,
    /// `PixelSizeX`, in micrometres.
    pub pixel_size_um: f64,
    /// `CameraXSize`.
    pub width: u32,
    /// `CameraYSize`.
    pub height: u32,
    pub max_adu: u32,
    /// `MaxBinX`.
    pub max_bin: u8,
    pub can_abort_exposure: bool,
}

/// The mount's static properties.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MountCapabilities {
    pub can_pulse_guide: bool,
}

pub type CameraState = DeviceState<CameraSensor>;
pub type MountState = DeviceState<MountCapabilities>;

/// What the poll knows of both devices. A device that is not configured
/// is `None`.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct DevicesState {
    pub camera: Option<CameraState>,
    pub mount: Option<MountState>,
}

/// A kind of device the guider is a client of.
trait Kind {
    type Device: Device + ?Sized;
    type Statics: Copy + Send + Sync;

    /// The kind as the guider's messages name it.
    const NAME: &'static str;

    /// `device` when it is of this kind.
    fn of(device: TypedDevice) -> Option<Arc<Self::Device>>;

    /// The device's static properties.
    fn read(
        device: Arc<Self::Device>,
    ) -> impl Future<Output = std::result::Result<Self::Statics, CallError>> + Send;
}

/// The guide camera.
struct CameraKind;

impl Kind for CameraKind {
    type Device = dyn Camera;
    type Statics = CameraSensor;

    const NAME: &'static str = "camera";

    fn of(device: TypedDevice) -> Option<Arc<dyn Camera>> {
        let TypedDevice::Camera(camera) = device else {
            return None;
        };
        Some(camera)
    }

    async fn read(camera: Arc<dyn Camera>) -> std::result::Result<CameraSensor, CallError> {
        let (sensor_type, pixel_size_um, width, height, max_adu, max_bin, can_abort_exposure) = tokio::try_join!(
            call(camera.sensor_type()),
            call(camera.pixel_size_x()),
            call(camera.camera_x_size()),
            call(camera.camera_y_size()),
            call(camera.max_adu()),
            call(camera.max_bin_x()),
            call(camera.can_abort_exposure()),
        )?;
        Ok(CameraSensor {
            sensor_type,
            pixel_size_um,
            width,
            height,
            max_adu,
            max_bin,
            can_abort_exposure,
        })
    }
}

/// The mount.
struct MountKind;

impl Kind for MountKind {
    type Device = dyn Telescope;
    type Statics = MountCapabilities;

    const NAME: &'static str = "mount";

    fn of(device: TypedDevice) -> Option<Arc<dyn Telescope>> {
        let TypedDevice::Telescope(telescope) = device else {
            return None;
        };
        Some(telescope)
    }

    async fn read(mount: Arc<dyn Telescope>) -> std::result::Result<MountCapabilities, CallError> {
        Ok(MountCapabilities {
            can_pulse_guide: call(mount.can_pulse_guide()).await?,
        })
    }
}

/// A client of one device: its server and its position among that
/// server's devices of its kind.
#[derive(Debug)]
pub struct DeviceClient {
    client: Client,
    device_number: u32,
}

impl DeviceClient {
    /// A client of the device at `address`, trusting `ca_cert` alone for an
    /// `https://` address when it is set.
    ///
    /// # Errors
    ///
    /// Returns [`GuiderError::DeviceClient`] if the HTTP client cannot be
    /// built.
    pub fn new(address: &DeviceAddress, ca_cert: Option<&Path>) -> Result<Self> {
        let url = address.alpaca_url.as_str();
        let failed = |message: String| GuiderError::DeviceClient {
            url: url.to_owned(),
            message,
        };
        let http = http_client(address, ca_cert).map_err(failed)?;
        let client = Client::new_with_client(url, http).map_err(|e| failed(e.to_string()))?;
        Ok(Self {
            client,
            device_number: address.device_number,
        })
    }

    /// The device of kind `K` at this client's position in its server's
    /// list, with the `UniqueID` listed for it: `None` when the list holds
    /// no such device.
    async fn listed<K: Kind>(
        &self,
    ) -> std::result::Result<Option<(Arc<K::Device>, String)>, CallError> {
        let devices = call(self.client.get_devices()).await?;
        let index = usize::try_from(self.device_number).unwrap_or(usize::MAX);
        Ok(devices.filter_map(K::of).nth(index).map(|device| {
            let unique_id = device.unique_id().trim().to_owned();
            (device, unique_id)
        }))
    }
}

/// The HTTP client of a device: its user agent, the connect timeout, the
/// server's Basic Auth credential, and trust in `ca_cert` alone when it is
/// set.
fn http_client(
    address: &DeviceAddress,
    ca_cert: Option<&Path>,
) -> std::result::Result<reqwest::Client, String> {
    let mut builder = rusty_photon_tls::client::client_builder(ca_cert)
        .map_err(|e| e.to_string())?
        .user_agent("rusty-photon-guider")
        .connect_timeout(CONNECT_TIMEOUT);
    if let Some(auth) = &address.auth {
        let encoded = BASE64.encode(format!("{}:{}", auth.username, auth.password));
        // Sensitive keeps the credential out of the client's `Debug`, which
        // prints its default headers.
        let mut value: reqwest::header::HeaderValue = format!("Basic {encoded}")
            .parse()
            .map_err(|e: reqwest::header::InvalidHeaderValue| e.to_string())?;
        value.set_sensitive(true);
        let mut headers = reqwest::header::HeaderMap::new();
        headers.insert(reqwest::header::AUTHORIZATION, value);
        builder = builder.default_headers(headers);
    }
    builder.build().map_err(|e| e.to_string())
}

/// One device under the poll, and whether its static properties have been
/// read since it last came back.
struct Polled<K: Kind> {
    client: DeviceClient,
    state: DeviceState<K::Statics>,
    fresh: bool,
}

impl<K: Kind> Polled<K> {
    fn new(client: DeviceClient) -> Self {
        Self {
            client,
            state: DeviceState::default(),
            fresh: false,
        }
    }

    /// Read the device once, and its static properties when it is
    /// connected and they have not been read since it came back.
    async fn poll(&mut self) {
        let before = self.state.reach;
        let reach = match self.client.listed::<K>().await {
            Err(error) => {
                debug!(device = K::NAME, %error, "the device's server does not answer");
                Reach::NoAnswer
            }
            Ok(None) => {
                self.state.unique_id = None;
                Reach::NotListed
            }
            Ok(Some((device, unique_id))) => {
                self.state.unique_id = Some(unique_id);
                match call(device.connected()).await {
                    Err(error) => {
                        debug!(device = K::NAME, %error, "the device does not answer");
                        Reach::NoAnswer
                    }
                    Ok(false) => Reach::NotConnected,
                    Ok(true) => {
                        if !self.fresh {
                            self.read_statics(device).await;
                        }
                        Reach::Connected
                    }
                }
            }
        };
        if reach != Reach::Connected {
            self.fresh = false;
        }
        if reach != before {
            debug!(device = K::NAME, from = ?before, to = ?reach, "the device's reach changed");
        }
        self.state.reach = reach;
    }

    async fn read_statics(&mut self, device: Arc<K::Device>) {
        match K::read(device).await {
            Ok(statics) => {
                self.state.statics = Some(statics);
                self.fresh = true;
            }
            Err(error) => {
                debug!(device = K::NAME, %error, "the device's static properties could not be read; the next poll reads them again");
            }
        }
    }
}

/// The poll of both devices, publishing what it reads.
pub struct DevicePoll {
    camera: Option<Polled<CameraKind>>,
    mount: Option<Polled<MountKind>>,
    interval: Duration,
    published: watch::Sender<DevicesState>,
}

impl DevicePoll {
    /// A poll of `camera` and `mount` every `interval`, with what it reads
    /// published to the returned receiver. A device that is not configured
    /// is `None`, and stays `None` in what is published.
    #[must_use]
    pub fn new(
        camera: Option<DeviceClient>,
        mount: Option<DeviceClient>,
        interval: Duration,
    ) -> (Self, watch::Receiver<DevicesState>) {
        let camera = camera.map(Polled::new);
        let mount = mount.map(Polled::new);
        let initial = DevicesState {
            camera: camera.as_ref().map(|polled| polled.state.clone()),
            mount: mount.as_ref().map(|polled| polled.state.clone()),
        };
        let (published, receiver) = watch::channel(initial);
        (
            Self {
                camera,
                mount,
                interval,
                published,
            },
            receiver,
        )
    }

    /// Poll until `stop` is cancelled: once at once, then every interval.
    pub async fn run(mut self, stop: CancellationToken) {
        let mut ticks = tokio::time::interval(self.interval);
        ticks.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        loop {
            tokio::select! {
                () = stop.cancelled() => break,
                _ = ticks.tick() => {}
            }
            tokio::select! {
                () = stop.cancelled() => break,
                () = self.poll() => {}
            }
        }
        debug!("device poll stopped");
    }

    /// Read both devices once, at the same time, and publish what was read.
    async fn poll(&mut self) {
        let Self {
            camera,
            mount,
            published,
            ..
        } = self;
        let camera_read = async {
            if let Some(polled) = camera.as_mut() {
                polled.poll().await;
            }
        };
        let mount_read = async {
            if let Some(polled) = mount.as_mut() {
                polled.poll().await;
            }
        };
        tokio::join!(camera_read, mount_read);
        published.send_replace(DevicesState {
            camera: camera.as_ref().map(|polled| polled.state.clone()),
            mount: mount.as_ref().map(|polled| polled.state.clone()),
        });
    }
}

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
mod tests {
    use axum::http::HeaderMap;
    use axum::routing::get;
    use axum::Router;
    use serde_json::json;

    use super::*;

    #[test]
    fn only_a_device_that_answers_is_reachable() {
        assert!(!Reach::NoAnswer.reachable());
        assert!(!Reach::NotListed.reachable());
        assert!(Reach::NotConnected.reachable());
        assert!(Reach::Connected.reachable());
    }

    #[test]
    fn only_a_connected_device_is_connected() {
        assert!(!Reach::NoAnswer.connected());
        assert!(!Reach::NotListed.connected());
        assert!(!Reach::NotConnected.connected());
        assert!(Reach::Connected.connected());
    }

    #[test]
    fn a_device_counts_as_not_answering_until_its_first_poll() {
        assert_eq!(CameraState::default().reach, Reach::NoAnswer);
    }

    /// A server that answers each request with the `Authorization` header
    /// it was sent.
    async fn echo_authorization() -> String {
        let router = Router::new().route(
            "/echo",
            get(|headers: HeaderMap| async move {
                headers
                    .get("authorization")
                    .and_then(|value| value.to_str().ok())
                    .unwrap_or("none")
                    .to_owned()
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
        format!("http://{addr}")
    }

    fn address(url: &str, auth: serde_json::Value) -> DeviceAddress {
        serde_json::from_value(json!({ "alpaca_url": url, "auth": auth })).unwrap()
    }

    #[tokio::test]
    async fn a_device_with_a_credential_is_sent_basic_auth() {
        let url = echo_authorization().await;
        let client = http_client(
            &address(
                &url,
                json!({ "username": "observatory", "password": "s3cret" }),
            ),
            None,
        )
        .unwrap();
        let sent = client
            .get(format!("{url}/echo"))
            .send()
            .await
            .unwrap()
            .text()
            .await
            .unwrap();
        assert_eq!(
            sent,
            format!("Basic {}", BASE64.encode("observatory:s3cret"))
        );
    }

    #[tokio::test]
    async fn a_device_without_a_credential_is_sent_none() {
        let url = echo_authorization().await;
        let client = http_client(&address(&url, json!(null)), None).unwrap();
        let sent = client
            .get(format!("{url}/echo"))
            .send()
            .await
            .unwrap()
            .text()
            .await
            .unwrap();
        assert_eq!(sent, "none");
    }

    #[test]
    fn the_credential_stays_out_of_the_clients_debug() {
        let client = http_client(
            &address(
                "http://127.0.0.1:1",
                json!({ "username": "observatory", "password": "s3cret" }),
            ),
            None,
        )
        .unwrap();
        let debug = format!("{client:?}");
        assert!(
            !debug.contains(&BASE64.encode("observatory:s3cret")),
            "{debug}"
        );
    }

    /// What the stub camera serves, which a test changes between polls.
    #[derive(Debug)]
    struct StubCamera {
        /// Whether the server lists the camera.
        listed: bool,
        connected: bool,
        width: u32,
        /// `MaxADU` reads that fail before one answers.
        max_adu_failures: usize,
    }

    type Stub = Arc<std::sync::Mutex<StubCamera>>;

    /// An Alpaca server listing one camera, answering its `Connected` and
    /// its static properties from `stub`.
    async fn stub_camera_server(stub: Stub) -> String {
        async fn member(
            axum::extract::State(stub): axum::extract::State<Stub>,
            axum::extract::Path(member): axum::extract::Path<String>,
        ) -> axum::Json<serde_json::Value> {
            let mut camera = stub.lock().unwrap();
            let value = match member.as_str() {
                "connected" => json!(camera.connected),
                "sensortype" => json!(0),
                "pixelsizex" => json!(3.76),
                "cameraxsize" => json!(camera.width),
                "cameraysize" => json!(1096),
                "maxadu" if camera.max_adu_failures > 0 => {
                    camera.max_adu_failures -= 1;
                    return axum::Json(json!({ "ErrorNumber": 1035, "ErrorMessage": "not now" }));
                }
                "maxadu" => json!(4095),
                "maxbinx" => json!(4),
                "canabortexposure" => json!(true),
                other => panic!("the stub serves no {other}"),
            };
            axum::Json(json!({ "Value": value, "ErrorNumber": 0, "ErrorMessage": "" }))
        }
        async fn configured_devices(
            axum::extract::State(stub): axum::extract::State<Stub>,
        ) -> axum::Json<serde_json::Value> {
            let listed = if stub.lock().unwrap().listed {
                json!([{
                    "DeviceName": "stub",
                    "DeviceType": "Camera",
                    "DeviceNumber": 0,
                    "UniqueID": "stub-camera"
                }])
            } else {
                json!([])
            };
            axum::Json(json!({ "Value": listed, "ErrorNumber": 0, "ErrorMessage": "" }))
        }
        let router = Router::new()
            .route("/management/v1/configureddevices", get(configured_devices))
            .route("/api/v1/camera/0/{member}", get(member))
            .with_state(stub);
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
        format!("http://{addr}")
    }

    async fn polled_stub(stub: &Stub) -> Polled<CameraKind> {
        let url = stub_camera_server(Arc::clone(stub)).await;
        Polled::new(DeviceClient::new(&address(&url, json!(null)), None).unwrap())
    }

    fn width(polled: &Polled<CameraKind>) -> Option<u32> {
        polled.state.statics.map(|sensor| sensor.width)
    }

    fn stub(connected: bool) -> Stub {
        Arc::new(std::sync::Mutex::new(StubCamera {
            listed: true,
            connected,
            width: 1936,
            max_adu_failures: 0,
        }))
    }

    #[tokio::test]
    async fn the_static_properties_are_read_once_while_a_device_stays_connected() {
        let stub = stub(true);
        let mut polled = polled_stub(&stub).await;
        polled.poll().await;
        assert_eq!(polled.state.reach, Reach::Connected);
        assert_eq!(polled.state.unique_id.as_deref(), Some("stub-camera"));
        assert_eq!(width(&polled), Some(1936));

        stub.lock().unwrap().width = 3840;
        polled.poll().await;
        assert_eq!(width(&polled), Some(1936));
    }

    #[tokio::test]
    async fn the_static_properties_are_read_again_when_a_device_comes_back() {
        let stub = stub(true);
        let mut polled = polled_stub(&stub).await;
        polled.poll().await;
        assert_eq!(width(&polled), Some(1936));

        stub.lock().unwrap().connected = false;
        polled.poll().await;
        assert_eq!(polled.state.reach, Reach::NotConnected);
        assert_eq!(width(&polled), Some(1936));

        stub.lock().unwrap().width = 3840;
        stub.lock().unwrap().connected = true;
        polled.poll().await;
        assert_eq!(polled.state.reach, Reach::Connected);
        assert_eq!(width(&polled), Some(3840));
    }

    #[tokio::test]
    async fn a_static_read_that_fails_is_tried_again_at_the_next_poll() {
        let stub = stub(true);
        stub.lock().unwrap().max_adu_failures = 1;
        let mut polled = polled_stub(&stub).await;
        polled.poll().await;
        assert_eq!(polled.state.reach, Reach::Connected);
        assert_eq!(width(&polled), None);

        polled.poll().await;
        assert_eq!(width(&polled), Some(1936));
    }

    #[tokio::test]
    async fn a_device_found_disconnected_has_no_static_property_read() {
        let stub = stub(false);
        let mut polled = polled_stub(&stub).await;
        polled.poll().await;
        assert_eq!(polled.state.reach, Reach::NotConnected);
        assert_eq!(width(&polled), None);
    }

    #[tokio::test]
    async fn a_device_its_server_stops_listing_loses_its_unique_id() {
        let stub = stub(true);
        let mut polled = polled_stub(&stub).await;
        polled.poll().await;
        assert_eq!(polled.state.unique_id.as_deref(), Some("stub-camera"));

        stub.lock().unwrap().listed = false;
        polled.poll().await;
        assert_eq!(polled.state.reach, Reach::NotListed);
        assert_eq!(polled.state.unique_id, None);
    }

    #[tokio::test(start_paused = true)]
    async fn a_call_with_no_answer_fails_at_the_deadline() {
        let never = std::future::pending::<std::result::Result<(), String>>();
        assert_eq!(call(never).await, Err(CallError::Deadline));
    }

    #[tokio::test]
    async fn a_call_that_answers_an_error_fails_with_it() {
        let failed = async { Err::<(), _>("refused") };
        assert_eq!(
            call(failed).await,
            Err(CallError::Failed("refused".to_owned()))
        );
    }
}
