#![cfg_attr(coverage_nightly, feature(coverage_attribute))]
#![cfg_attr(
    test,
    allow(
        clippy::arithmetic_side_effects,
        clippy::as_conversions,
        clippy::indexing_slicing
    )
)]
//! qhy-camera — ASCOM Alpaca **Camera** (+ optional **`FilterWheel`**) driver for
//! real QHYCCD hardware, built natively on the published `qhyccd-rs` crate.
//!
//! With no `usb_devices` list, the service enumerates every connected QHY
//! camera (and any CFW the probe finds on it) at startup and registers each as
//! an ASCOM device on one port, with a serial-derived `UniqueID`. With a list it
//! registers the list, in number order: the camera the USB join placed on each
//! entry's port, or a placeholder that says why there is none, and the wheel
//! each entry declares, at its own number — and a start or reload opens no
//! camera at all (`docs/services/qhy-camera.md` U1-U10). See
//! `docs/services/qhy-camera.md`.
//!
//! **Native dependency:** `qhyccd-rs → libqhyccd-sys` links the proprietary QHYCCD
//! SDK (`static=qhyccd`) + `libusb-1.0`, so this package does not build without
//! the SDK installed. The `simulation` feature makes the backend hardware-free
//! (`Sdk::new()` fabricates a QHY178M-Simulated camera) but still links the SDK.

// Curated test-scope allow list — documented in the root Cargo.toml [workspace.lints] block.
#![cfg_attr(
    test,
    allow(
        clippy::needless_pass_by_ref_mut,
        clippy::needless_pass_by_value,
        clippy::unused_async,
        clippy::unused_async_trait_impl,
        clippy::used_underscore_binding,
        clippy::significant_drop_tightening,
        clippy::significant_drop_in_scrutinee,
        clippy::cast_possible_truncation,
        clippy::cast_sign_loss,
        clippy::cast_precision_loss,
        clippy::cast_possible_wrap,
        clippy::suboptimal_flops,
        clippy::too_many_lines,
        clippy::option_if_let_else,
        clippy::match_same_arms,
        clippy::float_cmp,
        clippy::similar_names,
        clippy::struct_excessive_bools,
    )
)]

pub mod backend;
pub mod camera;
pub mod claims;
pub mod config;
pub mod config_actions;
pub mod doctor;
pub mod error;
pub mod filterwheel;
pub mod preflight;

pub use camera::QhyCameraDevice;
pub use claims::UsbSource;
pub use config::{
    load_effective_config, read_effective_config, AlpacaServerConfig, CliOverrides, Config,
    DeviceOverride, UsbDeviceEntry,
};
pub use config_actions::QhyCameraDriver;
pub use error::{QhyCameraError, Result};
pub use filterwheel::{PlaceholderFilterWheel, QhyFilterWheelDevice};
pub use rusty_photon_driver::claims::{serve_and_rebuild, Ended};

use std::future::Future;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;

use ascom_alpaca::api::CargoServerInfo;
use ascom_alpaca::Server;
use rusty_photon_doctor_checks::claims::{Claims, Resolution};
use rusty_photon_driver::claims::Rescan;
use rusty_photon_driver::ConfigActionCtx;
use rusty_photon_service_lifecycle::ReloadSignal;
use rusty_photon_tls::config::TlsConfig;
use tracing::{debug, error, info, warn};

use crate::backend::{
    CameraHandle, FilterWheelHandle, QhyCameraHandle, QhyFilterWheelHandle, SharedCameraConnection,
};

/// What every device this service registers answers for `DriverInfo`,
/// placeholders included.
pub const DRIVER_INFO: &str = "rusty-photon qhy-camera";

/// The errors `build` and `start` return.
type BoxError = Box<dyn std::error::Error + Send + Sync>;

/// Builder for the qhy-camera ASCOM Alpaca server.
///
/// On [`build`](ServerBuilder::build) it registers the devices this
/// configuration serves and binds the HTTP listener — returning a
/// [`BoundServer`] that keeps the SDK alive for the serve.
#[derive(Default)]
pub struct ServerBuilder {
    config: Config,
    config_path: Option<PathBuf>,
    overrides: CliOverrides,
    reload: Option<ReloadSignal>,
    /// SDK injected by `main` (the only use is the test-only simulation
    /// backends); when `None`, `build()` opens the SDK via `Sdk::new()`.
    injected_sdk: Option<qhyccd_rs::Sdk>,
    /// Where a `usb_devices` list's USB scan comes from (U7); `None` takes
    /// this driver's default ([`claims::default_usb_source`]).
    usb_source: Option<UsbSource>,
}

impl ServerBuilder {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    #[must_use]
    pub fn with_config(mut self, config: Config) -> Self {
        self.config = config;
        self
    }

    /// Set the config source (persist path + CLI overrides) for the config
    /// actions. Together with [`Self::with_reload_signal`], enables editing.
    #[must_use]
    pub fn with_config_source(mut self, path: PathBuf, overrides: CliOverrides) -> Self {
        self.config_path = Some(path);
        self.overrides = overrides;
        self
    }

    /// Provide the reload trigger `config.apply` fires after its response flushes.
    #[must_use]
    pub fn with_reload_signal(mut self, reload: ReloadSignal) -> Self {
        self.reload = Some(reload);
        self
    }

    /// Inject a pre-constructed SDK (used to select a simulation backend).
    /// With a `usb_devices` list it is the SDK's side of the join, so an empty
    /// one leaves every listed port's record unplaced (U7).
    #[must_use]
    pub fn with_sdk(mut self, sdk: qhyccd_rs::Sdk) -> Self {
        self.injected_sdk = Some(sdk);
        self
    }

    /// Take a `usb_devices` list's USB scan from `source` (U7) — a staged
    /// document, in a simulation build. The default is the host in a release
    /// build and the simulated camera's record in a simulation build.
    #[cfg(feature = "simulation")]
    #[must_use]
    pub fn with_usb_source(mut self, source: UsbSource) -> Self {
        self.usb_source = Some(source);
        self
    }

    /// Where this build's USB scan comes from.
    fn usb_source(&self) -> UsbSource {
        self.usb_source
            .clone()
            .unwrap_or_else(claims::default_usb_source)
    }

    /// Register the devices this configuration serves and bind the listener.
    ///
    /// With no `usb_devices` list, every camera the SDK enumerates, in SDK
    /// order, each probed for a CFW without `InitQHYCCD` (C0, C5). With a list,
    /// the list, opening nothing (see [`Self::register_list`]). A camera's
    /// connect handshake happens later, on `set_connected(true)`; the device
    /// list is fixed at build, so a reload re-enumerates. Zero cameras is not
    /// a hard failure (C0).
    ///
    /// # Errors
    ///
    /// Returns [`QhyCameraError::Config`] if the list breaks a rule,
    /// [`QhyCameraError::Sdk`] if the SDK cannot be initialized or enumerate,
    /// [`QhyCameraError::Bind`] if the listener cannot be bound or its address
    /// read, and the responder's bind error if the config opts into discovery
    /// and that port is taken. A failed USB scan is not an error: it is served
    /// as placeholders and retried (U6).
    pub async fn build(mut self) -> std::result::Result<BoundServer, BoxError> {
        // The load refuses a list that breaks a rule; checked again here
        // because registration depends on it — numbers 0..N-1 are what make
        // each entry's position its number — and a config built in code never
        // passed through a load.
        self.config.check_list().map_err(QhyCameraError::Config)?;
        let injected = self.injected_sdk.take();
        let mut server = Server::new(CargoServerInfo!());
        server.listen_addr = self.config.server.socket_addr();
        let (registered, sdk) = match &self.config.usb_devices {
            None => self.register_enumerated(&mut server, injected).await?,
            Some(list) => self.register_list(&mut server, list, injected).await?,
        };

        let tls = self.config.server.tls.clone();
        let router = axum::Router::new().fallback_service(server.into_service());

        // HTTP Basic Auth (config `server.auth`); absent means unauthenticated.
        let router = match &self.config.server.auth {
            Some(auth) => {
                if self.config.server.tls.is_none() {
                    warn!(
                        "Authentication is enabled but TLS is not. \
                         Credentials will be transmitted in cleartext. \
                         Consider enabling TLS (see `doctor --fix`)."
                    );
                }
                rp_auth::layer(router, auth)
            }
            None => router,
        };

        // Shared dual-stack helper (IPv6 + IPv4) with SO_REUSEADDR, like every
        // other Alpaca service. SO_REUSEADDR matters here because the in-process
        // reload loop rebinds the same port; a raw bind could fail to rebind
        // while a prior listener's TIME_WAIT lingers.
        let bind_addr = self.config.server.socket_addr();
        let listener = rusty_photon_tls::server::bind_dual_stack_tokio(bind_addr)
            .await
            .map_err(|source| QhyCameraError::Bind {
                port: self.config.server.port,
                source,
            })?;
        let local_addr = listener.local_addr().map_err(|e| QhyCameraError::Bind {
            port: self.config.server.port,
            source: rusty_photon_tls::error::TlsError::Io(e),
        })?;

        // Opt-in Alpaca UDP discovery responder (config `discovery_port`);
        // bound here so a taken port fails startup, run in start().
        let discovery =
            rusty_photon_driver::discovery::bind(local_addr, self.config.server.discovery_port)
                .await?;

        // Load-bearing: `bdd_infra::ServiceHandle` greps stdout for `bound_addr=`.
        // Console mode only: stdout is a dead handle under the Windows SCM,
        // and the only stdout consumer (bdd-infra's port parser) never runs
        // services with --service.
        if !rusty_photon_service_lifecycle::is_scm_service() {
            println!("Bound Alpaca server bound_addr={local_addr}");
        }
        info!(
            cameras = registered.cameras,
            filter_wheels = registered.filter_wheels,
            placeholders = registered.placeholders,
            "Service started successfully on {local_addr}"
        );

        Ok(BoundServer {
            listener,
            router,
            local_addr,
            tls,
            discovery,
            rescan: registered.rescan,
            sdk,
        })
    }

    /// The no-list default: open the SDK, probe every enumerated camera for a
    /// CFW, and register each camera the probe could open, in SDK order, with
    /// the wheel the probe found on it.
    async fn register_enumerated(
        &self,
        server: &mut Server,
        injected: Option<qhyccd_rs::Sdk>,
    ) -> std::result::Result<(Registered, Option<qhyccd_rs::Sdk>), BoxError> {
        // The open and the probe are SDK calls, a USB round trip each, so they
        // run on the blocking pool.
        let (sdk, unprobed) = tokio::task::spawn_blocking(move || {
            let mut sdk = take_sdk(injected)?;
            let unprobed = probe_every_camera(&mut sdk);
            Ok::<_, QhyCameraError>((sdk, unprobed))
        })
        .await??;

        // Ids of cameras that report a filter wheel (each CFW wraps a Camera with
        // the same SDK id). Every discovered CFW is registered — detection is the
        // source of truth, exactly as for cameras (no opt-in toggle). Used to pair
        // a camera with its CFW so both ASCOM devices share ONE physical
        // connection — disconnecting either device must not tear down the other's
        // handle (see SharedCameraConnection).
        let cfw_ids: std::collections::HashSet<String> =
            sdk.filter_wheels().map(|w| w.id().to_string()).collect();
        let cameras: Vec<qhyccd_rs::Camera> = sdk
            .cameras()
            .filter(|camera| !unprobed.contains(camera.id()))
            .cloned()
            .collect();
        let mut registered = Registered::default();
        for camera in cameras {
            let id = camera.id().to_string();
            let conn = self.register_camera(server, camera, self.config.devices.get(&id));
            registered.cameras = registered.cameras.saturating_add(1);
            if cfw_ids.contains(&id) {
                // The CFW shares the camera's SDK id, so the per-serial override's
                // `name`/`description` belong to the camera, not the wheel — only
                // `filter_names` applies here. The wheel keeps its derived name.
                let filter_names = self
                    .config
                    .devices
                    .get(&id)
                    .and_then(|d| d.filter_names.clone());
                register_wheel(server, conn, filter_names);
                registered.filter_wheels = registered.filter_wheels.saturating_add(1);
            }
        }
        if registered.cameras == 0 && unprobed.is_empty() {
            warn!("no QHY cameras discovered; starting with no Camera devices");
        } else if registered.cameras == 0 {
            warn!(
                enumerated = unprobed.len(),
                "every QHY camera the SDK enumerated failed its CFW probe (see the warnings \
                 above); starting with no Camera devices"
            );
        }
        Ok((registered, Some(sdk)))
    }

    /// A `usb_devices` list: take the USB scan, enumerate the SDK — each
    /// camera's id and model, before any open — place each SDK camera on a
    /// port, and register the list: each entry in number order as the camera
    /// placed on its port (U5) or a placeholder that says why there is none
    /// (U4), then each declared wheel in wheel-number order, on its camera's
    /// connection or as a placeholder with its camera's reason (U10). Nothing
    /// is opened, and a camera on an unlisted port is never registered (U1).
    /// Returns what was registered, the re-scan to run when the scan failed
    /// (U6), and the SDK, which a failed scan never opens.
    async fn register_list(
        &self,
        server: &mut Server,
        list: &[UsbDeviceEntry],
        injected: Option<qhyccd_rs::Sdk>,
    ) -> std::result::Result<(Registered, Option<qhyccd_rs::Sdk>), BoxError> {
        if list.is_empty() {
            warn!(
                "usb_devices is an empty list: no QHY camera is registered, every one is left \
                 to other applications"
            );
            return Ok((Registered::default(), None));
        }
        let (scan, sdk, cameras) = self.scan_and_enumerate(injected).await?;
        let claims = Claims::new(
            claims::NORMALIZER,
            claims::SERVICE,
            scan,
            claims::sdk_cameras(&cameras),
        );
        for (index, port) in claims.placed() {
            if !list.iter().any(|e| e.usb_port == port) {
                debug!(
                    camera = cameras.get(index).map_or("", |c| c.id.as_str()),
                    port, "a QHY camera on an unlisted port is neither opened nor served"
                );
            }
        }

        let mut registered = Registered::default();
        let mut wheels = Vec::new();
        for (entry, outcome) in resolve_list(&claims, list) {
            let camera = outcome.and_then(|index| {
                sdk.as_ref()
                    .and_then(|sdk| sdk.cameras().nth(index).cloned())
                    .ok_or_else(|| {
                        format!("the join placed SDK camera {index}, which is not enumerated")
                    })
            });
            let wheel = match camera {
                Ok(camera) => {
                    debug!(
                        device = entry.device_number,
                        port = %entry.usb_port,
                        camera = %camera.id(),
                        "a listed port holds a QHY camera"
                    );
                    let conn =
                        self.register_camera(server, camera, Some(&DeviceOverride::from(&entry)));
                    registered.cameras = registered.cameras.saturating_add(1);
                    Ok(conn)
                }
                Err(reason) => {
                    warn!(
                        device = entry.device_number,
                        port = %entry.usb_port,
                        %reason,
                        "a listed number is held by a placeholder"
                    );
                    let mut placeholder = claims::placeholder(&entry, reason.clone());
                    if let Some(ctx) = self.config_actions() {
                        placeholder = placeholder.with_config_actions(ctx);
                    }
                    server.devices.register(placeholder);
                    registered.placeholders = registered.placeholders.saturating_add(1);
                    Err(reason)
                }
            };
            if let Some(number) = entry.filter_wheel_number {
                wheels.push((number, entry, wheel));
            }
        }
        wheels.sort_by_key(|(number, _, _)| *number);
        for (number, entry, wheel) in wheels {
            match wheel {
                Ok(conn) => {
                    debug!(filter_wheel = number, port = %entry.usb_port, "a declared filter wheel is served on its camera's connection");
                    register_wheel(server, conn, entry.filter_names.clone());
                    registered.filter_wheels = registered.filter_wheels.saturating_add(1);
                }
                Err(reason) => {
                    debug!(filter_wheel = number, port = %entry.usb_port, "a declared filter wheel is held by a placeholder, as its camera is");
                    server
                        .devices
                        .register(claims::placeholder_wheel(&entry, reason));
                    registered.placeholders = registered.placeholders.saturating_add(1);
                }
            }
        }
        if claims.scan_error().is_some() {
            registered.rescan = Some(self.rescan());
        }
        Ok((registered, sdk))
    }

    /// A list's USB scan, and — only once the scan runs (U6) — the SDK,
    /// opened, and its cameras' ids and models, read before any open (U1).
    async fn scan_and_enumerate(
        &self,
        injected: Option<qhyccd_rs::Sdk>,
    ) -> std::result::Result<Scanned, BoxError> {
        let source = self.usb_source();
        // A collector that panics has failed to scan, like one that errors:
        // placeholders and a re-scan, never a failed start (U6).
        let scan = tokio::task::spawn_blocking(move || source.scan())
            .await
            .unwrap_or_else(|error| Err(format!("the USB scan did not finish: {error}")));
        if let Err(error) = &scan {
            error!(
                %error,
                "the USB scan failed; every listed number is a placeholder until a re-scan \
                 succeeds"
            );
            return Ok((scan, None, Vec::new()));
        }
        let (sdk, cameras) = tokio::task::spawn_blocking(move || {
            let sdk = take_sdk(injected)?;
            let cameras = claims::enumerate(&sdk);
            Ok::<_, QhyCameraError>((sdk, cameras))
        })
        .await??;
        Ok((scan, Some(sdk), cameras))
    }

    /// Register `camera` with its display overrides, and return the physical
    /// connection a wheel on it shares.
    fn register_camera(
        &self,
        server: &mut Server,
        camera: qhyccd_rs::Camera,
        overrides: Option<&DeviceOverride>,
    ) -> Arc<SharedCameraConnection> {
        let id = camera.id().to_string();
        // One shared physical connection per camera; the Camera device and (if
        // present) the CFW device both hold it and refcount open/close.
        let conn = SharedCameraConnection::new(camera);
        let handle: Arc<dyn CameraHandle> = Arc::new(QhyCameraHandle::new(Arc::clone(&conn)));
        let mut device = QhyCameraDevice::new(handle, overrides);
        if let Some(ctx) = self.config_actions() {
            device = device.with_config_actions(ctx);
        }
        server.devices.register(device);
        debug!(camera = %id, "registered Camera device");
        conn
    }

    /// The config-action context every registered camera carries, when the
    /// builder has a config source and a reload path.
    fn config_actions(&self) -> Option<ConfigActionCtx<QhyCameraDriver>> {
        let (path, reload) = (self.config_path.clone()?, self.reload.clone()?);
        Some(ConfigActionCtx {
            effective: self.config.clone(),
            path,
            overrides: self.overrides.clone(),
            reload,
        })
    }

    /// The background re-scan after a failed USB scan (U6): each try takes
    /// this build's scan and, once that runs, checks the SDK enumerates; on
    /// the first try both pass,
    /// it ends its own server, which the caller rebuilds from the
    /// configuration it was serving ([`Ended::Rescanned`]), placing the listed
    /// cameras afresh.
    fn rescan(&self) -> Rescan {
        let source = self.usb_source();
        Rescan::new(
            claims::SDK_NAME,
            move || source.scan().map(drop),
            || {
                // A failed scan's server holds no SDK, so another `Sdk` alive
                // in this process means this try outlived its server and the
                // next one is serving: `try_new` answers rather than waits.
                qhyccd_rs::Sdk::try_new()
                    .map(drop)
                    .map_err(|e| format!("{e:#}"))
            },
        )
    }
}

/// Register a `FilterWheel` on `conn`, the physical connection of the camera
/// it hangs off.
fn register_wheel(
    server: &mut Server,
    conn: Arc<SharedCameraConnection>,
    filter_names: Option<Vec<String>>,
) {
    let id = conn.camera().id().to_string();
    let handle: Arc<dyn FilterWheelHandle> = Arc::new(QhyFilterWheelHandle::new(conn));
    server
        .devices
        .register(QhyFilterWheelDevice::new(handle, filter_names, None));
    debug!(filter_wheel = %id, "registered FilterWheel device");
}

/// Each entry of `list`, in number order, with the SDK camera the join placed
/// on its port — by its enumeration index — or the reason a placeholder holds
/// its number (U4).
fn resolve_list(
    claims: &Claims<'_>,
    list: &[UsbDeviceEntry],
) -> Vec<(UsbDeviceEntry, std::result::Result<usize, String>)> {
    let mut entries = list.to_vec();
    entries.sort_by_key(|e| e.device_number);
    entries
        .into_iter()
        .map(|entry| {
            let outcome = match claims.resolve(&entry.usb_port) {
                Resolution::Camera(index) => Ok(index),
                Resolution::Placeholder(reason) => Err(reason),
            };
            (entry, outcome)
        })
        .collect()
}

/// A list's USB scan, the SDK it opened when the scan ran, and the cameras
/// that SDK enumerated.
type Scanned = (
    std::result::Result<rusty_photon_doctor_checks::UsbScan, String>,
    Option<qhyccd_rs::Sdk>,
    Vec<claims::Enumerated>,
);

/// What a build registered, and the re-scan to run when the USB scan failed.
#[derive(Default)]
struct Registered {
    cameras: usize,
    filter_wheels: usize,
    placeholders: usize,
    rescan: Option<Rescan>,
}

/// The SDK a server's devices run on: `injected`, or the SDK opened —
/// waiting, if a re-scan's try has one alive (`qhyccd-rs` makes one `Sdk` at a
/// time in a process), for that try to finish. Blocking.
fn take_sdk(
    injected: Option<qhyccd_rs::Sdk>,
) -> std::result::Result<qhyccd_rs::Sdk, QhyCameraError> {
    if let Some(sdk) = injected {
        return Ok(sdk);
    }
    let sdk = match qhyccd_rs::Sdk::try_new() {
        Err(qhyccd_rs::QHYError::SdkInUse) => {
            // Only a failed scan's re-scan can hold it here: a try that
            // outlived its server, still in its SDK check (U6).
            warn!("waiting for a USB re-scan's QHY SDK check to finish before opening the SDK");
            qhyccd_rs::Sdk::new()
        }
        tried => tried,
    };
    sdk.map_err(|e| QhyCameraError::Sdk(format!("{e:#}")))
}

/// Probe every enumerated camera for a CFW. Returns the ids of the cameras the
/// probe could not open or close, which are not registered.
///
/// A camera whose close failed keeps its handle, so this process holds it
/// without serving it until the SDK is released — on reload or shutdown, when
/// `Sdk`'s drop closes it again. `still_open` in the warning says which case it
/// was.
fn probe_every_camera(sdk: &mut qhyccd_rs::Sdk) -> std::collections::HashSet<String> {
    let ids: Vec<String> = sdk.cameras().map(|c| c.id().to_string()).collect();
    let mut unprobed = std::collections::HashSet::new();
    for (id, outcome) in sdk.probe_filter_wheels(&ids) {
        if matches!(outcome, Ok(false)) {
            // A wheel the plug query misses three times is not registered;
            // a usb_devices entry declaring it is the remedy (U10).
            debug!(camera = %id, "the CFW probe found no filter wheel on this camera");
        }
        if let Err(error) = outcome {
            let still_open = sdk
                .cameras()
                .any(|camera| camera.id() == id && matches!(camera.is_open(), Ok(true)));
            warn!(
                camera = %id,
                %error,
                still_open,
                "camera not registered: the CFW probe could not open or close it \
                 (one still open is held until the service reloads or stops)"
            );
            unprobed.insert(id);
        }
    }
    unprobed
}

/// A fully bound qhy-camera server ready to accept connections.
pub struct BoundServer {
    listener: tokio::net::TcpListener,
    router: axum::Router,
    local_addr: SocketAddr,
    /// TLS settings (config `server.tls`); `None` serves plain HTTP.
    tls: Option<TlsConfig>,
    /// Alpaca UDP discovery responder, when the config opts in. Runs inside
    /// `start()`'s select so its socket closes when serving ends (reload).
    discovery: Option<ascom_alpaca::discovery::BoundDiscoveryServer>,
    /// The re-scan of a failed USB scan (U6), run while this server serves.
    rescan: Option<Rescan>,
    /// Held so the SDK is released (its `Drop` calls `ReleaseQHYCCDResource` on
    /// real hardware) only after the HTTP server stops — on shutdown *and*
    /// reload. `None` when a list's failed scan, or an empty list, opened none.
    sdk: Option<qhyccd_rs::Sdk>,
}

impl BoundServer {
    pub const fn listen_addr(&self) -> SocketAddr {
        self.local_addr
    }

    /// Serve until `shutdown` resolves — or, after a failed USB scan, until
    /// the re-scan finds a rebuild would serve the list (U6) — then release
    /// the SDK.
    ///
    /// Returns which of the two ended it: on [`Ended::Rescanned`] the caller
    /// rebuilds the server, as for a reload.
    ///
    /// # Errors
    ///
    /// Returns [`QhyCameraError::Server`] if the TLS material cannot be loaded
    /// or the serve loop fails.
    pub async fn start(
        self,
        shutdown: impl Future<Output = ()> + Send + 'static,
    ) -> std::result::Result<Ended, BoxError> {
        let Self {
            listener,
            router,
            local_addr: _,
            tls,
            discovery,
            rescan,
            sdk,
        } = self;
        // The re-scan's success ends this server and no other: it arrives on
        // this server's own channel rather than on the service-wide reload
        // signal, so a success landing after another reload has already
        // stopped this server reaches nobody and cannot stop the next one.
        let (shutdown, cause) = rusty_photon_driver::claims::stop_or_rescanned(shutdown, rescan);
        let serve = async {
            let result = if let Some(ref tls_config) = tls {
                debug!("serving over TLS");
                rusty_photon_tls::server::serve_tls(listener, router, tls_config, shutdown).await
            } else {
                debug!("serving plain HTTP");
                rusty_photon_tls::server::serve_plain(listener, router, shutdown).await
            };
            result.map_err(|e| QhyCameraError::Server(e.to_string()))
        };
        let result = rusty_photon_driver::discovery::serve_with(discovery, serve).await;
        debug!("qhy-camera shut down");
        // The SDK is released after serving completes.
        drop(sdk);
        result?;
        Ok(cause.ended())
    }
}

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
mod list_tests {
    use super::*;
    use rusty_photon_doctor_checks::{UsbDevice, UsbScan};

    fn record(product: &str, port: &str) -> UsbDevice {
        UsbDevice {
            vendor: "1618".to_string(),
            product: product.to_string(),
            model: None,
            port: Some(port.to_string()),
            serial: None,
        }
    }

    fn entry(device_number: u32, usb_port: &str) -> UsbDeviceEntry {
        UsbDeviceEntry {
            device_number,
            usb_port: usb_port.to_string(),
            name: None,
            description: None,
            filter_wheel_number: None,
            filter_names: None,
        }
    }

    fn joined(scan: std::result::Result<UsbScan, String>, models: &[&str]) -> Claims<'static> {
        let cameras: Vec<claims::Enumerated> = models
            .iter()
            .map(|model| claims::Enumerated {
                id: format!("{model}-1"),
                model: (*model).to_string(),
            })
            .collect();
        Claims::new(
            claims::NORMALIZER,
            claims::SERVICE,
            scan,
            claims::sdk_cameras(&cameras),
        )
    }

    #[test]
    fn entries_resolve_in_number_order_to_the_camera_placed_on_their_port() {
        let claims = joined(
            Ok(UsbScan {
                devices: vec![record("c601", "p1"), record("0679", "p2")],
                faults: Vec::new(),
            }),
            &["QHY5III678M", "QHY600M"],
        );
        let resolved = resolve_list(&claims, &[entry(1, "p2"), entry(0, "p1")]);
        let outcomes: Vec<(u32, std::result::Result<usize, String>)> = resolved
            .into_iter()
            .map(|(e, outcome)| (e.device_number, outcome))
            .collect();
        assert_eq!(outcomes, vec![(0, Ok(1)), (1, Ok(0))]);
    }

    #[test]
    fn an_entry_whose_port_holds_no_camera_resolves_to_its_placeholders_reason() {
        let claims = joined(
            Ok(UsbScan {
                devices: vec![record("c601", "p1")],
                faults: Vec::new(),
            }),
            &["QHY600M"],
        );
        let resolved = resolve_list(&claims, &[entry(0, "p9")]);
        let reason = resolved[0].1.clone().unwrap_err();
        assert!(
            reason.starts_with("no working camera is enumerated on p9"),
            "{reason}"
        );
        assert!(reason.contains("reload or restart qhy-camera"), "{reason}");
    }

    #[test]
    fn every_entry_of_a_failed_scan_resolves_to_its_error() {
        let claims = joined(Err("powershell.exe did not answer".to_string()), &[]);
        let resolved = resolve_list(&claims, &[entry(0, "p1"), entry(1, "p2")]);
        for (_, outcome) in resolved {
            let reason = outcome.unwrap_err();
            assert!(
                reason.starts_with("the USB scan failed: powershell.exe did not answer"),
                "{reason}"
            );
        }
    }
}

/// A server's build and an `Sdk` still alive in the process (U6): a re-scan
/// try that outlived its server.
#[cfg(all(test, feature = "simulation"))]
#[cfg_attr(coverage_nightly, coverage(off))]
mod take_sdk_tests {
    use super::*;
    use std::sync::mpsc;
    use std::time::Duration;

    #[test]
    fn a_build_waits_for_a_live_sdk_to_drop_before_opening_its_own() {
        let try_sdk = qhyccd_rs::Sdk::new().unwrap();
        let (taken_tx, taken) = mpsc::channel();
        let build = std::thread::spawn(move || {
            let sdk = take_sdk(None).unwrap();
            taken_tx.send(()).unwrap();
            drop(sdk);
        });
        assert_eq!(
            taken.recv_timeout(Duration::from_millis(100)),
            Err(mpsc::RecvTimeoutError::Timeout),
            "the build opened an SDK beside a live one"
        );
        drop(try_sdk);
        taken.recv_timeout(Duration::from_secs(30)).unwrap();
        build.join().unwrap();
    }

    #[test]
    fn an_injected_sdk_is_taken_as_it_is() {
        let mut injected = qhyccd_rs::Sdk::new_simulated();
        injected.add_simulated_camera(
            qhyccd_rs::simulation::SimulatedCameraConfig::default().with_id("INJECTED"),
        );
        let sdk = take_sdk(Some(injected)).unwrap();
        let ids: Vec<&str> = sdk.cameras().map(qhyccd_rs::Camera::id).collect();
        assert_eq!(ids, ["INJECTED"]);
    }
}
