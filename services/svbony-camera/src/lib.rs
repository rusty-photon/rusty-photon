#![cfg_attr(coverage_nightly, feature(coverage_attribute))]
#![cfg_attr(
    test,
    allow(
        clippy::arithmetic_side_effects,
        clippy::as_conversions,
        clippy::indexing_slicing
    )
)]
//! # svbony-camera — ASCOM Alpaca driver for `SVBony` cameras
//!
//! **Phase E scope (this crate today).** The service binary builds, binds
//! the Alpaca listener on port 11125, and serves `/management/*` correctly
//! with zero or one registered device; `doctor` genuinely diagnoses config +
//! SDK reachability. [`camera::SvbonyCamera`] implements both `Device` and
//! `ascom_alpaca::api::Camera` for real — connection lifecycle, config
//! actions, sensor geometry/type, gain/offset/readout, binning/ROI,
//! cooling, and the soft-trigger video-capture exposure state machine
//! (incl. abort and pulse-guide) — with `ElectronsPerADU` the one
//! permanent `NOT_IMPLEMENTED` stub (no native SDK field). See
//! `docs/services/svbony-camera.md` for the full design.
//!
//! ## Native dependency
//!
//! `svbony-rs` links exactly `libSVBCameraSDK` (+ `libusb-1.0`) — machines
//! compiling this package need that SDK installed, even with the
//! `simulation` feature, which removes the *camera*, not the *link* (see
//! `SVBONY_SKIP_NATIVE_LINK` in `crates/svbony-rs/libsvbony-sys/build.rs`).
//!
//! ## Device registration
//!
//! With no `usb_devices` list, `build()` enumerates whatever
//! `svbony_rs::Sdk::cameras()` reports and registers each camera as an ASCOM
//! device. With the `simulation` feature that is `svbony-rs`'s one fabricated
//! `SV605CC-Simulated` camera (so BDD scenarios have "camera device 0" to
//! address); the production real-SDK build registers the physically connected
//! cameras. With a list it registers the list, in number order: the camera the
//! USB join placed on each entry's port, or a placeholder that says why there
//! is none (`docs/services/svbony-camera.md` U1-U9).

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
mod camera;
pub mod claims;
mod config;
mod config_actions;
pub mod doctor;
mod error;
mod placeholder;

pub use camera::SvbonyCamera;
pub use claims::UsbSource;
pub use config::{
    load_effective_config, read_effective_config, AlpacaServerConfig, CliOverrides, Config,
    DeviceOverride, UsbDeviceEntry, DEFAULT_PORT,
};
pub use config_actions::SvbonyCameraDriver;
pub use error::SvbonyCameraError;
pub use placeholder::PlaceholderCamera;

use std::future::Future;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;

use ascom_alpaca::api::{CargoServerInfo, Device};
use ascom_alpaca::Server;
use rusty_photon_doctor_checks::claims::{Claims, Resolution};
use rusty_photon_driver::ConfigActionCtx;
use rusty_photon_service_lifecycle::ReloadSignal;
use rusty_photon_tls::config::TlsConfig;
use svbony_rs::CameraInfo;
use tokio::net::TcpListener;
use tracing::{debug, error, info, warn};

use crate::backend::{CameraHandle, SvbonyCameraHandle};

/// One camera discovered at enumeration: its index, [`CameraInfo`], the bare
/// SDK `serial` (the key for `devices` config overrides), and the
/// serial-derived ASCOM `UniqueID`.
struct EnumeratedCamera {
    index: usize,
    info: CameraInfo,
    serial: String,
    unique_id: String,
}

/// Builds a bound svbony-camera server from an effective [`Config`].
#[derive(Default)]
pub struct ServerBuilder {
    config: Config,
    config_path: Option<PathBuf>,
    overrides: CliOverrides,
    reload: Option<ReloadSignal>,
    /// Register no cameras regardless of what enumeration would otherwise
    /// report — the test-only zero-camera startup path, mirroring
    /// `zwo-camera`'s `--simulation-empty` (contract C0). With a
    /// `usb_devices` list it empties only the SDK's side of the join.
    force_empty: bool,
    /// Where a `usb_devices` list's USB scan comes from (U7).
    usb_source: UsbSource,
    /// The file whose existence takes the simulated cameras off the bus — the
    /// test-only path exercising a camera that loses its power while connected
    /// (contract C6). `None` builds cameras that never leave.
    #[cfg(feature = "simulation")]
    departure_file: Option<PathBuf>,
}

impl ServerBuilder {
    /// Create a builder with default configuration.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Set the effective configuration to serve.
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

    /// Register no cameras regardless of what the SDK reports — the test-only
    /// empty-backend path exercising the zero-camera startup (contract C0).
    #[must_use]
    pub const fn with_empty(mut self, empty: bool) -> Self {
        self.force_empty = empty;
        self
    }

    /// Take a `usb_devices` list's USB scan from `source` (U7) — a staged
    /// document, in a simulation build. The default is the host in a release
    /// build and the simulated cameras' records in a simulation build.
    #[cfg(feature = "simulation")]
    #[must_use]
    pub fn with_usb_source(mut self, source: UsbSource) -> Self {
        self.usb_source = source;
        self
    }

    /// Build the simulated cameras so they leave the bus whenever `path`
    /// exists, and come back when it is removed (`svbony-rs`'s
    /// `Sdk::with_departure_file`) — the test-only path exercising a camera
    /// that loses its power while connected (contract C6). `None` builds
    /// cameras that never leave.
    #[cfg(feature = "simulation")]
    #[must_use]
    pub fn with_departure_file(mut self, path: Option<PathBuf>) -> Self {
        self.departure_file = path;
        self
    }

    /// `sdk` with this builder's departure file, when there is one.
    #[cfg(feature = "simulation")]
    fn departing(&self, sdk: svbony_rs::Sdk) -> svbony_rs::Sdk {
        if let Some(path) = &self.departure_file {
            return sdk.with_departure_file(path);
        }
        sdk
    }

    /// Register the cameras this configuration serves and bind the Alpaca
    /// listener.
    ///
    /// With no `usb_devices` list, every camera the SDK enumerates, in SDK
    /// order; zero discovered cameras is **not** a hard failure: the server
    /// starts with no Camera devices and logs a warning (a later reload
    /// re-enumerates). With a list, the list (see [`Self::register_list`]).
    ///
    /// # Errors
    /// Returns [`SvbonyCameraError`] when SDK enumeration fails or the
    /// listener cannot bind the configured port. A failed USB scan is not an
    /// error: it is served as placeholders and retried (U6).
    pub async fn build(self) -> Result<BoundServer, SvbonyCameraError> {
        // The load refuses a list that breaks a rule; checked again here
        // because registration depends on it — numbers 0..N-1 are what make
        // each entry's position its number — and a config built in code
        // never passed through a load.
        self.config
            .check_list()
            .map_err(SvbonyCameraError::Config)?;
        let mut server = Server::new(CargoServerInfo!());
        let registered = match &self.config.usb_devices {
            None => Registered {
                cameras: self.register_enumerated(&mut server).await?,
                placeholders: 0,
                rescan: None,
            },
            Some(list) => self.register_list(&mut server, list).await?,
        };

        // Use the shared dual-stack helper (IPv6 + IPv4) with SO_REUSEADDR, like
        // every other Alpaca service. SO_REUSEADDR matters here because the
        // in-process `with_reload` loop rebinds the same port; a raw bind could
        // fail to rebind while a prior listener's TIME_WAIT lingers.
        let bind_addr = self.config.server.socket_addr();
        let listener = rusty_photon_tls::server::bind_dual_stack_tokio(bind_addr)
            .await
            .map_err(|source| SvbonyCameraError::Bind {
                addr: bind_addr.to_string(),
                source,
            })?;
        let local_addr = listener
            .local_addr()
            .map_err(|source| SvbonyCameraError::Bind {
                addr: bind_addr.to_string(),
                source: rusty_photon_tls::error::TlsError::Io(source),
            })?;

        // Opt-in Alpaca UDP discovery responder (config `discovery_port`);
        // bound here so a taken port fails startup, run in start().
        let discovery =
            rusty_photon_driver::discovery::bind(local_addr, self.config.server.discovery_port)
                .await
                .map_err(|e| SvbonyCameraError::Discovery(e.to_string()))?;

        let tls = self.config.server.tls.clone();
        let app = axum::Router::new().fallback_service(server.into_service());

        // HTTP Basic Auth (config `server.auth`); absent means unauthenticated.
        let app = match &self.config.server.auth {
            Some(auth) => {
                if self.config.server.tls.is_none() {
                    warn!(
                        "Authentication is enabled but TLS is not. \
                         Credentials will be transmitted in cleartext. \
                         Consider enabling TLS (see `doctor --fix`)."
                    );
                }
                rp_auth::layer(app, auth)
            }
            None => app,
        };

        // Stdout is reserved for the machine-readable `bound_addr=<host>:<port>`
        // handshake that `bdd-infra::parse_bound_port` waits on for port
        // discovery. Console mode only: stdout is a dead handle under the
        // Windows SCM, and the only stdout consumer never runs services with
        // `--service`.
        if !rusty_photon_service_lifecycle::is_scm_service() {
            println!("Bound Alpaca server bound_addr={local_addr}");
        }
        info!(
            cameras = registered.cameras,
            placeholders = registered.placeholders,
            address = %local_addr,
            "Service started successfully"
        );
        Ok(BoundServer {
            listener,
            app,
            local_addr,
            tls,
            discovery,
            rescan: registered.rescan,
        })
    }

    /// The no-list default: every camera the SDK enumerates, in SDK order.
    /// Returns how many were registered.
    async fn register_enumerated(&self, server: &mut Server) -> Result<usize, SvbonyCameraError> {
        let cameras = if self.force_empty {
            Vec::new()
        } else {
            enumerate_cameras().await?
        };
        if cameras.is_empty() {
            warn!("no SVBony cameras registered; starting with no Camera devices");
        }
        for cam in &cameras {
            // `devices` overrides are keyed by the bare SDK serial (matching
            // the config-actions `devices.{serial}` paths), NOT the prefixed
            // `SVBONY:{name}:{serial}` UniqueID.
            self.register_camera(server, cam, self.config.devices.get(&cam.serial))?;
        }
        Ok(cameras.len())
    }

    /// A `usb_devices` list: take the USB scan, enumerate the SDK, place each
    /// SDK camera on a port, and register each entry in number order — the
    /// camera placed on its port (U5), or a placeholder that says why there
    /// is none (U4). Nothing is opened. Returns what was registered, and the
    /// re-scan to run when the scan failed (U6).
    async fn register_list(
        &self,
        server: &mut Server,
        list: &[UsbDeviceEntry],
    ) -> Result<Registered, SvbonyCameraError> {
        if list.is_empty() {
            warn!(
                "usb_devices is an empty list: no SVBony camera is registered, every one is \
                 left to other applications"
            );
            return Ok(Registered::default());
        }
        let source = self.usb_source.clone();
        // A collector that panics has failed to scan, like one that errors:
        // placeholders and a re-scan, never a failed start (U6).
        let scan = tokio::task::spawn_blocking(move || source.scan())
            .await
            .unwrap_or_else(|error| Err(format!("the USB scan did not finish: {error}")));
        let cameras = match &scan {
            Err(error) => {
                error!(
                    %error,
                    "the USB scan failed; every listed number is a placeholder until a re-scan \
                     succeeds"
                );
                Vec::new()
            }
            Ok(_) if self.force_empty => Vec::new(),
            Ok(_) => enumerate_cameras().await?,
        };
        let claims = Claims::new(
            claims::NORMALIZER,
            claims::SERVICE,
            scan,
            claims::sdk_cameras(cameras.iter().map(|c| &c.info)),
        );

        let mut registered = Registered::default();
        let mut entries: Vec<&UsbDeviceEntry> = list.iter().collect();
        entries.sort_by_key(|e| e.device_number);
        for entry in entries {
            let placed = match claims.resolve(&entry.usb_port) {
                Resolution::Camera(index) => cameras.get(index).ok_or_else(|| {
                    format!("the join placed SDK camera {index}, which is not enumerated")
                }),
                Resolution::Placeholder(reason) => Err(reason),
            };
            match placed {
                Ok(cam) => {
                    debug!(
                        device = entry.device_number,
                        port = %entry.usb_port,
                        "a listed port holds an SVBony camera"
                    );
                    self.register_camera(server, cam, Some(&entry.display_override()))?;
                    registered.cameras = registered.cameras.saturating_add(1);
                }
                Err(reason) => {
                    warn!(
                        device = entry.device_number,
                        port = %entry.usb_port,
                        %reason,
                        "a listed number is held by a placeholder"
                    );
                    let mut placeholder = PlaceholderCamera::new(entry, reason);
                    if let Some(ctx) = self.config_actions() {
                        placeholder = placeholder.with_config_actions(ctx);
                    }
                    server.devices.register(placeholder);
                    registered.placeholders = registered.placeholders.saturating_add(1);
                }
            }
        }
        for (index, port) in claims.placed() {
            if !list.iter().any(|e| e.usb_port == port) {
                debug!(
                    camera = index,
                    port, "an SVBony camera on an unlisted port is not served"
                );
            }
        }

        if claims.scan_error().is_some() {
            registered.rescan = Some(Rescan {
                source: self.usb_source.clone(),
                config: self
                    .config_path
                    .clone()
                    .map(|path| (path, self.overrides.clone())),
            });
        }
        Ok(registered)
    }

    /// Register one enumerated camera with its display overrides.
    fn register_camera(
        &self,
        server: &mut Server,
        cam: &EnumeratedCamera,
        overrides: Option<&DeviceOverride>,
    ) -> Result<(), SvbonyCameraError> {
        // Said here rather than where the identity is minted: with a list,
        // the SDK's cameras on unlisted ports are enumerated too, and one
        // another application owns is no concern of this service's log.
        if cam.info.serial.is_empty() {
            warn!(
                camera = %cam.info.friendly_name,
                "camera reports an empty serial; using a position-based identity"
            );
        }
        let sdk = svbony_rs::Sdk::new()?;
        #[cfg(feature = "simulation")]
        let sdk = self.departing(sdk);
        let handle: Arc<dyn CameraHandle> = Arc::new(SvbonyCameraHandle::new(
            sdk,
            cam.info.clone(),
            cam.unique_id.clone(),
        ));
        let mut device = SvbonyCamera::new(handle, overrides);
        if let Some(ctx) = self.config_actions() {
            device = device.with_config_actions(ctx);
        }
        debug!(sdk_index = cam.index, name = %device.static_name(), "registering SVBony camera");
        server.devices.register(device);
        Ok(())
    }

    /// The config-action context every registered device carries, when the
    /// builder has a config source and a reload path.
    fn config_actions(&self) -> Option<ConfigActionCtx<SvbonyCameraDriver>> {
        let (path, reload) = (self.config_path.clone()?, self.reload.clone()?);
        Some(ConfigActionCtx {
            effective: self.config.clone(),
            path,
            overrides: self.overrides.clone(),
            reload,
        })
    }
}

/// What a build registered: the cameras it serves, the placeholders holding
/// listed numbers, and the re-scan to run when the USB scan failed.
#[derive(Default)]
struct Registered {
    cameras: usize,
    placeholders: usize,
    rescan: Option<Rescan>,
}

/// The background re-scan after a failed USB scan (U6): it waits 10 s, 20 s,
/// 40 s and then 60 s between tries, and on the first after which a rebuild
/// would serve the list it ends its own server, which the caller rebuilds
/// ([`Ended::Rescanned`]) — placing the listed cameras afresh.
#[derive(Clone)]
struct Rescan {
    source: UsbSource,
    /// The config file the rebuild will read, and the CLI overrides layered
    /// on it. `None` when the builder has no config source.
    config: Option<(PathBuf, CliOverrides)>,
}

/// What still stands between a re-scan and the rebuild it would ask for.
#[derive(Debug)]
enum Blocker {
    /// The USB scan still fails.
    Scan(String),
    /// The config file no longer loads, and a rebuild that cannot load its
    /// config ends the service.
    Config(String),
    /// The SDK cannot enumerate, and a rebuild that cannot enumerate ends the
    /// service: with the first scan failed, the rebuild is the first
    /// enumeration since the start.
    Sdk(String),
}

impl Blocker {
    /// Whether this blocker is news to the operator after `previous`, the
    /// one the try before met: a config file or SDK that blocks the rebuild is
    /// warned about each time it starts to, so one that clears and comes back
    /// — or gives way to the other — is said again, while one that persists
    /// from the try before is not.
    fn is_new(&self, previous: Option<&Self>) -> bool {
        previous.is_none_or(|p| std::mem::discriminant(p) != std::mem::discriminant(self))
    }
}

impl Rescan {
    /// Re-scan until a rebuild would serve the list, then say so on `ready`.
    /// Nobody asked for this rebuild, so it waits out anything that would make
    /// it end the service instead.
    async fn run(self, ready: tokio::sync::oneshot::Sender<()>) {
        let mut previous = None;
        for wait in rusty_photon_doctor_checks::claims::rescan_waits() {
            tokio::time::sleep(wait).await;
            let this = self.clone();
            let blocker = match tokio::task::spawn_blocking(move || this.blocker()).await {
                Ok(None) => {
                    info!("the USB scan succeeds again; rebuilding to place the listed cameras");
                    // A send to a server that has already stopped goes
                    // nowhere: the success belongs to this server alone.
                    let _ = ready.send(());
                    return;
                }
                Ok(Some(blocker)) => blocker,
                Err(error) => {
                    debug!(%error, "the USB re-scan task failed");
                    continue;
                }
            };
            let new = blocker.is_new(previous.as_ref());
            match &blocker {
                Blocker::Scan(error) => debug!(%error, "the USB scan still fails"),
                Blocker::Config(error) if new => {
                    warn!(%error, "the config file no longer loads; not rebuilding until it does");
                }
                Blocker::Sdk(error) if new => {
                    warn!(
                        %error,
                        "the USB scan succeeds again, but the SVBony SDK cannot enumerate; not \
                         rebuilding until it can"
                    );
                }
                Blocker::Config(error) | Blocker::Sdk(error) => {
                    debug!(%error, "the rebuild is still blocked");
                }
            }
            previous = Some(blocker);
        }
    }

    /// What would keep a rebuild now from serving the list, checked cheapest
    /// first — the config file, then the scan, then the SDK's enumeration —
    /// or `None` when nothing would. Blocking.
    fn blocker(&self) -> Option<Blocker> {
        if let Some((path, overrides)) = &self.config {
            if let Err(error) = load_effective_config(path, overrides) {
                return Some(Blocker::Config(error.to_string()));
            }
        }
        if let Err(error) = self.source.scan() {
            return Some(Blocker::Scan(error));
        }
        sdk_camera_infos()
            .err()
            .map(|error| Blocker::Sdk(error.to_string()))
    }
}

/// Why [`BoundServer::start`] returned.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Ended {
    /// The caller's stop future resolved.
    Stopped,
    /// A failed USB scan succeeds again (U6): rebuild the server, as for a
    /// reload, to place the listed cameras.
    Rescanned,
}

/// A svbony-camera server bound to a local port, ready to [`start`](Self::start).
pub struct BoundServer {
    listener: TcpListener,
    app: axum::Router,
    local_addr: SocketAddr,
    /// TLS settings (config `server.tls`); `None` serves plain HTTP.
    tls: Option<TlsConfig>,
    /// Alpaca UDP discovery responder, when the config opts in. Runs inside
    /// `start()`'s select so its socket closes when serving ends (reload).
    discovery: Option<ascom_alpaca::discovery::BoundDiscoveryServer>,
    /// The re-scan of a failed USB scan (U6), run while this server serves.
    rescan: Option<Rescan>,
}

impl BoundServer {
    /// The address the listener is bound to (useful when the port was `0`).
    #[must_use]
    pub const fn local_addr(&self) -> SocketAddr {
        self.local_addr
    }

    /// Serve until `shutdown` resolves — or, after a failed USB scan, until
    /// the re-scan finds a rebuild would serve the list (U6) — then drain
    /// gracefully.
    ///
    /// Returns which of the two ended it: on [`Ended::Rescanned`] the caller
    /// rebuilds the server, as for a reload.
    ///
    /// # Errors
    /// Returns [`SvbonyCameraError::Server`] if the HTTP server stops with an error.
    pub async fn start(
        self,
        shutdown: impl Future<Output = ()> + Send + 'static,
    ) -> Result<Ended, SvbonyCameraError> {
        let Self {
            listener,
            app,
            local_addr: _,
            tls,
            discovery,
            rescan,
        } = self;
        // The re-scan's success ends this server and no other: it arrives on
        // this server's own channel rather than on the service-wide reload
        // signal, so a success landing after another reload has already
        // stopped this server reaches nobody and cannot stop the next one.
        // Dropping the stop future, wherever serving ends, aborts it.
        let (ready_tx, ready_rx) = tokio::sync::oneshot::channel();
        let rescan = rescan.map(|rescan| {
            tokio_util::task::AbortOnDropHandle::new(tokio::spawn(rescan.run(ready_tx)))
        });
        let rescanned = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let shutdown = {
            let rescanned = Arc::clone(&rescanned);
            async move {
                let ready = async {
                    if rescan.is_some() && ready_rx.await.is_ok() {
                        return;
                    }
                    std::future::pending::<()>().await;
                };
                tokio::select! {
                    () = shutdown => {}
                    () = ready => rescanned.store(true, std::sync::atomic::Ordering::SeqCst),
                }
                drop(rescan);
            }
        };
        let serve = async {
            let result = if let Some(ref tls_config) = tls {
                debug!("serving over TLS");
                rusty_photon_tls::server::serve_tls(listener, app, tls_config, shutdown).await
            } else {
                debug!("serving plain HTTP");
                rusty_photon_tls::server::serve_plain(listener, app, shutdown).await
            };
            result.map_err(|e| SvbonyCameraError::Server(e.to_string()))
        };
        rusty_photon_driver::discovery::serve_with(discovery, serve).await?;
        if rescanned.load(std::sync::atomic::Ordering::SeqCst) {
            return Ok(Ended::Rescanned);
        }
        Ok(Ended::Stopped)
    }
}

/// Enumerate connected `SVBony` cameras, minting each device's serial-derived
/// `UniqueID`.
///
/// Unlike ZWO (`ASIGetSerialNumber` requires an open camera), `SVBony`'s
/// `CameraSN` arrives with `SVBGetCameraInfo` at enumeration time
/// ([`svbony_rs::Sdk::cameras`]) — no open-then-close dance is needed to mint
/// identity. Enumeration reads properties without opening any camera, so it
/// never actuates hardware and never contends with a connected client.
async fn enumerate_cameras() -> Result<Vec<EnumeratedCamera>, SvbonyCameraError> {
    let cameras = tokio::task::spawn_blocking(enumerate_cameras_blocking).await??;
    debug!(count = cameras.len(), "enumerated SVBony cameras");
    Ok(cameras)
}

fn enumerate_cameras_blocking() -> Result<Vec<EnumeratedCamera>, svbony_rs::Error> {
    Ok(sdk_camera_infos()?
        .into_iter()
        .enumerate()
        .map(|(index, info)| {
            let (serial, unique_id) = mint_identity(&info, index);
            EnumeratedCamera {
                index,
                info,
                serial,
                unique_id,
            }
        })
        .collect())
}

/// What the SDK enumerates, before any open: the one enumeration the service,
/// `doctor` and `doctor --devices` all read.
pub(crate) fn sdk_camera_infos() -> Result<Vec<CameraInfo>, svbony_rs::Error> {
    svbony_rs::Sdk::new()?.cameras()
}

/// Mint the `(serial, UniqueID)` pair for an enumerated camera.
///
/// The serial is the camera's hardware `CameraSN`, read pre-open at
/// enumeration (unlike ZWO, no open-to-mint-identity dance is needed — see
/// [`enumerate_cameras`]'s doc comment). A camera reporting an empty serial
/// falls back to a stable position-based identity (`noserial-{index}`),
/// mirroring `zwo-camera`'s `mint_identity` fallback.
fn mint_identity(info: &CameraInfo, index: usize) -> (String, String) {
    let serial = claims::override_key(info, index);
    let unique_id = format!("SVBONY:{}:{}", info.friendly_name.replace(' ', "-"), serial);
    (serial, unique_id)
}

#[cfg(test)]
mod identity_tests {
    use super::mint_identity;
    use svbony_rs::CameraInfo;

    fn info(friendly_name: &str, serial: &str) -> CameraInfo {
        CameraInfo {
            id: 0,
            friendly_name: friendly_name.to_string(),
            serial: serial.to_string(),
            port_type: "USB3".to_string(),
            device_id: 0,
        }
    }

    #[test]
    fn mint_identity_uses_the_enumeration_time_serial_when_present() {
        let (serial, unique_id) = mint_identity(&info("SV605CC", "SVB0123456789AB"), 0);
        assert_eq!(serial, "SVB0123456789AB");
        assert_eq!(unique_id, "SVBONY:SV605CC:SVB0123456789AB");
    }

    #[test]
    fn mint_identity_falls_back_to_position_when_serial_is_empty() {
        let (serial, unique_id) = mint_identity(&info("SV605CC", ""), 2);
        assert_eq!(serial, "noserial-2");
        assert_eq!(unique_id, "SVBONY:SV605CC:noserial-2");
    }

    #[test]
    fn mint_identity_replaces_spaces_in_the_friendly_name() {
        let (_, unique_id) = mint_identity(&info("SV605 CC Pro", "ABC"), 0);
        assert_eq!(unique_id, "SVBONY:SV605-CC-Pro:ABC");
    }
}

#[cfg(all(test, feature = "simulation"))]
#[cfg_attr(coverage_nightly, coverage(off))]
mod simulation_tests {
    use super::*;

    /// End-to-end proof against the `svbony-rs` simulation backend: the
    /// builder enumerates the one simulated camera, registers it, and binds
    /// an ephemeral port.
    #[tokio::test]
    async fn builds_and_binds_against_the_simulation_backend() {
        let config: Config = serde_json::from_str(r#"{"server":{"port":0}}"#).unwrap();
        let bound = ServerBuilder::new()
            .with_config(config)
            .build()
            .await
            .unwrap();
        assert_ne!(bound.local_addr().port(), 0);
    }

    /// `devices` overrides are keyed by the bare SDK serial, not the prefixed
    /// `SVBONY:{name}:{serial}` `UniqueID`.
    #[tokio::test]
    async fn device_overrides_are_keyed_by_serial_not_unique_id() {
        let cameras = enumerate_cameras().await.unwrap();
        let cam = &cameras[0];
        assert!(
            cam.serial != cam.unique_id && cam.unique_id.ends_with(&cam.serial),
            "UniqueID should be the prefixed serial, serial the bare key"
        );
        let mut config = Config::default();
        config.devices.insert(
            cam.serial.clone(),
            DeviceOverride {
                name: Some("Main Imaging".to_string()),
                ..Default::default()
            },
        );
        assert!(config.devices.contains_key(&cam.serial));
        assert!(!config.devices.contains_key(&cam.unique_id));
    }

    fn list(entries: &[(u32, &str)]) -> Vec<UsbDeviceEntry> {
        entries
            .iter()
            .map(|&(device_number, usb_port)| UsbDeviceEntry {
                device_number,
                usb_port: usb_port.to_string(),
                name: None,
                description: None,
            })
            .collect()
    }

    /// Register `entries` and read back each Camera's `UniqueID`, in device
    /// number order, with what the build says it registered.
    async fn register(
        builder: &ServerBuilder,
        entries: &[(u32, &str)],
    ) -> Result<(Vec<String>, Registered), SvbonyCameraError> {
        let mut server = Server::new(CargoServerInfo!());
        let registered = builder.register_list(&mut server, &list(entries)).await?;
        let ids: Vec<String> = server
            .devices
            .iter::<dyn ascom_alpaca::api::Camera>()
            .map(|camera| camera.unique_id().to_string())
            .collect();
        assert_eq!(ids.len(), registered.cameras + registered.placeholders);
        Ok((ids, registered))
    }

    /// A staged inventory file holding `document`, kept alive by the guard.
    fn staged(document: &str) -> std::io::Result<(tempfile::TempDir, UsbSource)> {
        let dir = tempfile::tempdir()?;
        let path = dir.path().join("usb-inventory.json");
        std::fs::write(&path, document)?;
        Ok((dir, UsbSource::Staged(path)))
    }

    const FAILED_SCAN: &str = r#"{"usb_unavailable": "powershell.exe timed out"}"#;

    #[tokio::test]
    async fn a_list_registers_its_entries_in_number_order() {
        let (ids, registered) = register(
            &ServerBuilder::new(),
            &[(1, "simulated-usbv3-0:1"), (0, "simulated-usbv3-0:9")],
        )
        .await
        .unwrap();
        assert_eq!(
            ids,
            vec![
                "placeholder:svbony-camera:simulated-usbv3-0:9",
                "SVBONY:SV605CC-Simulated:SVB0123456789AB",
            ]
        );
        assert_eq!((registered.cameras, registered.placeholders), (1, 1));
        assert!(
            registered.rescan.is_none(),
            "a scan that ran needs no re-scan"
        );
    }

    #[tokio::test]
    async fn an_empty_list_registers_nothing_and_scans_nothing() {
        // A scan of this source would fail; an empty list never takes one.
        let builder = ServerBuilder::new()
            .with_usb_source(UsbSource::Staged(PathBuf::from(
                "/nonexistent/inventory.json",
            )))
            .with_reload_signal(ReloadSignal::new());
        let (ids, registered) = register(&builder, &[]).await.unwrap();
        assert_eq!(ids, Vec::<String>::new());
        assert!(registered.rescan.is_none(), "an empty list scans nothing");
    }

    #[tokio::test]
    async fn a_failed_scan_holds_every_number_and_is_retried() {
        let (_dir, source) = staged(FAILED_SCAN).unwrap();
        let builder = ServerBuilder::new().with_usb_source(source);
        let (ids, registered) = register(&builder, &[(0, "simulated-usbv3-0:1")])
            .await
            .unwrap();
        assert_eq!(ids, vec!["placeholder:svbony-camera:simulated-usbv3-0:1"]);
        assert_eq!((registered.cameras, registered.placeholders), (0, 1));
        registered
            .rescan
            .expect("a failed scan schedules a re-scan");
    }

    #[tokio::test]
    async fn an_empty_backend_leaves_a_listed_record_unplaced() {
        let builder = ServerBuilder::new().with_empty(true);
        let (ids, _) = register(&builder, &[(0, "simulated-usbv3-0:1")])
            .await
            .unwrap();
        assert_eq!(ids, vec!["placeholder:svbony-camera:simulated-usbv3-0:1"]);
    }

    /// Run `rescan` for up to `budget` of paused time, and say whether it
    /// reported a rebuild would serve the list.
    async fn rescan_ready_within(rescan: Rescan, budget: std::time::Duration) -> bool {
        let (ready_tx, mut ready_rx) = tokio::sync::oneshot::channel();
        let _ = tokio::time::timeout(budget, rescan.run(ready_tx)).await;
        ready_rx.try_recv().is_ok()
    }

    #[tokio::test(start_paused = true)]
    async fn a_rescan_that_succeeds_says_the_server_can_rebuild() {
        let rescan = Rescan {
            source: UsbSource::Simulated,
            config: None,
        };
        assert!(rescan_ready_within(rescan, std::time::Duration::from_secs(300)).await);
    }

    #[tokio::test(start_paused = true)]
    async fn a_rescan_never_asks_for_a_rebuild_while_the_scan_fails() {
        let (_dir, source) = staged(FAILED_SCAN).unwrap();
        let rescan = Rescan {
            source,
            config: None,
        };
        assert!(
            !rescan_ready_within(rescan, std::time::Duration::from_secs(300)).await,
            "a failing scan asked for a rebuild"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn a_rescan_never_asks_for_a_rebuild_into_a_config_file_that_no_longer_loads() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("svbony-camera.json");
        std::fs::write(
            &path,
            r#"{"usb_devices": [{"device_number": 1, "usb_port": "a"}]}"#,
        )
        .unwrap();
        let rescan = Rescan {
            source: UsbSource::Simulated,
            config: Some((path, CliOverrides::default())),
        };
        assert!(
            !rescan_ready_within(rescan, std::time::Duration::from_secs(300)).await,
            "the re-scan asked for a rebuild into a config file that does not load"
        );
    }

    #[test]
    fn a_rescan_checks_the_config_file_before_it_scans() {
        // A scan of this source would fail; a config that does not load is
        // what the blocker names, so no scan was taken.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("svbony-camera.json");
        std::fs::write(
            &path,
            r#"{"usb_devices": [{"device_number": 1, "usb_port": "a"}]}"#,
        )
        .unwrap();
        let rescan = Rescan {
            source: UsbSource::Staged(PathBuf::from("/nonexistent/inventory.json")),
            config: Some((path, CliOverrides::default())),
        };
        let blocker = rescan.blocker();
        assert!(matches!(blocker, Some(Blocker::Config(_))), "{blocker:?}");
    }

    #[test]
    fn a_blocker_is_news_each_time_it_starts() {
        let config = || Blocker::Config("broken".to_string());
        let scan = || Blocker::Scan("no sysfs".to_string());
        let sdk = || Blocker::Sdk("no SDK".to_string());
        assert!(config().is_new(None), "the first try's");
        assert!(!config().is_new(Some(&config())), "one that persists");
        assert!(
            config().is_new(Some(&scan())),
            "one that clears and comes back"
        );
        assert!(
            sdk().is_new(Some(&config())),
            "one that gives way to the other"
        );
        assert!(config().is_new(Some(&sdk())), "and back again");
    }

    #[tokio::test]
    async fn a_build_refuses_a_list_that_breaks_a_rule() {
        let config = Config {
            usb_devices: Some(list(&[(1, "simulated-usbv3-0:1")])),
            ..Config::default()
        };
        let error = ServerBuilder::new()
            .with_config(config)
            .build()
            .await
            .err()
            .unwrap()
            .to_string();
        assert!(error.contains("0 is missing"), "{error}");
    }

    /// The empty-backend path starts healthy with no Camera devices (C0).
    #[tokio::test]
    async fn empty_backend_binds_with_no_cameras() {
        let config: Config = serde_json::from_str(r#"{"server":{"port":0}}"#).unwrap();
        let bound = ServerBuilder::new()
            .with_config(config)
            .with_empty(true)
            .build()
            .await
            .unwrap();
        assert_ne!(bound.local_addr().port(), 0);
    }
}

#[cfg(all(test, not(feature = "simulation")))]
#[cfg_attr(coverage_nightly, coverage(off))]
mod production_default_tests {
    use super::*;

    /// The production (non-`simulation`) build's `build()` registers exactly
    /// the cameras `svbony_rs::Sdk::cameras()` reports. Under `cargo test`
    /// the dev-dependency turns on `svbony-rs/simulation` via feature
    /// unification, so the *production* enumeration path here sees the one
    /// simulated camera — the same code path that registers physical
    /// cameras in the real-SDK binary.
    #[tokio::test]
    async fn production_build_enumerates_via_the_sdk() {
        let cameras = enumerate_cameras().await.unwrap();
        assert_eq!(cameras.len(), svbony_rs::SIM_CAMERA_COUNT);
        let config: Config = serde_json::from_str(r#"{"server":{"port":0}}"#).unwrap();
        let bound = ServerBuilder::new()
            .with_config(config)
            .build()
            .await
            .unwrap();
        assert_ne!(bound.local_addr().port(), 0);
    }
}
