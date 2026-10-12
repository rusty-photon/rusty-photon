#![cfg_attr(coverage_nightly, feature(coverage_attribute))]
#![cfg_attr(
    test,
    allow(
        clippy::arithmetic_side_effects,
        clippy::as_conversions,
        clippy::indexing_slicing
    )
)]
//! # zwo-camera — ASCOM Alpaca driver for ZWO ASI cameras
//!
//! With no `usb_devices` list, the service enumerates every connected ASI
//! camera via [`zwo_rs`] and registers each as an ASCOM [`ZwoCamera`] device on
//! one port, with a serial-derived `UniqueID`. With a list it registers the
//! list, in number order: the camera the USB join placed on each entry's port,
//! or a placeholder that says why there is none — and it opens only the
//! cameras on listed ports (`docs/services/zwo-camera.md` U1-U9). Each device
//! implements the full `Device` + `Camera` surface (exposure state machine,
//! ROI/binning, gain/offset, cooling, readout, ST4 pulse-guiding) over the
//! [`backend::CameraHandle`] SDK seam. EFW filter wheels and EAF focusers are
//! separate services (ADR-014): each independently usable ZWO device gets its
//! own driver process.
//!
//! See `docs/services/zwo-camera.md` for the full design and
//! `docs/plans/zwo-driver.md` for the decision record.
//!
//! ## Native dependency
//!
//! `zwo-rs` is built with only its `camera` feature (ADR-014), so this binary
//! links exactly `libASICamera2` (+ `libusb-1.0`) — machines compiling this
//! package need that SDK installed, even with the `simulation` feature, which
//! removes the *camera*, not the *link*.

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

pub use camera::ZwoCamera;
pub use claims::UsbSource;
pub use config::{
    load_effective_config, read_effective_config, AlpacaServerConfig, CliOverrides, Config,
    DeviceOverride, UsbDeviceEntry, DEFAULT_PORT,
};
pub use config_actions::ZwoCameraDriver;
pub use error::ZwoCameraError;
pub use rusty_photon_driver::claims::Ended;

use std::future::Future;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;

use ascom_alpaca::api::{CargoServerInfo, Device};
use ascom_alpaca::Server;
use rusty_photon_doctor_checks::claims::{Claims, Resolution};
use rusty_photon_doctor_checks::UsbScan;
use rusty_photon_driver::claims::{first_blocker, Blocker, Rescan};
use rusty_photon_driver::ConfigActionCtx;
use rusty_photon_service_lifecycle::ReloadSignal;
use rusty_photon_tls::config::TlsConfig;
use tokio::net::TcpListener;
use tracing::{debug, error, info, warn};
use zwo_rs::CameraInfo;

use crate::backend::{CameraHandle, HeldCameras, ZwoCameraHandle};

/// One camera discovered at enumeration: its index, [`CameraInfo`], the bare
/// SDK `serial` (the key for `devices` config overrides), and the
/// serial-derived ASCOM `UniqueID`.
#[derive(Debug, Clone)]
struct EnumeratedCamera {
    index: usize,
    info: CameraInfo,
    /// The hardware serial, or the `noserial-{index}` stand-in; the key the
    /// `devices` overrides use.
    serial: String,
    /// The hardware serial alone, which an open matches its camera by (C6).
    hardware_serial: Option<String>,
    unique_id: String,
}

/// Builds a bound zwo-camera server from an effective [`Config`].
#[derive(Default)]
pub struct ServerBuilder {
    config: Config,
    config_path: Option<PathBuf>,
    overrides: CliOverrides,
    reload: Option<ReloadSignal>,
    /// Register no cameras (the test-only zero-camera startup path, C0). With
    /// a `usb_devices` list it empties only the SDK's side of the join.
    force_empty: bool,
    /// Where a `usb_devices` list's USB scan comes from (U7); `None` takes
    /// this driver's default ([`claims::default_usb_source`]).
    usb_source: Option<UsbSource>,
    /// Test-only: the file whose existence takes the simulated camera off
    /// the bus (C6). See [`Self::with_departure_file`].
    #[cfg(feature = "simulation")]
    departure_file: Option<PathBuf>,
    /// Test-only: every open of the simulated camera fails (U5). See
    /// [`Self::with_unopenable`].
    #[cfg(feature = "simulation")]
    unopenable: bool,
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
        self.usb_source = Some(source);
        self
    }

    /// Test-only: give every registered camera's SDK `zwo-rs`'s departure
    /// file, so the simulated camera leaves the bus while `path` exists and
    /// returns when it is removed — the camera that loses its power or cable
    /// while connected (C6). `None` keeps a camera that never leaves. The
    /// start's enumeration and identity opens go through the same switches
    /// ([`Self::simulated`]), so a file present at a start hides the camera
    /// from them too; the scenarios create it after startup.
    #[cfg(feature = "simulation")]
    #[must_use]
    pub fn with_departure_file(mut self, path: Option<PathBuf>) -> Self {
        self.departure_file = path;
        self
    }

    /// Test-only: every open of the simulated camera fails with the SDK's
    /// `GENERAL_ERROR` (`zwo-rs`'s `Sdk::with_unopenable`) — the identity open
    /// at startup (C0, U5) and a client's connect alike.
    #[cfg(feature = "simulation")]
    #[must_use]
    pub const fn with_unopenable(mut self, unopenable: bool) -> Self {
        self.unopenable = unopenable;
        self
    }

    /// Where this build's USB scan comes from.
    fn usb_source(&self) -> UsbSource {
        self.usb_source
            .clone()
            .unwrap_or_else(claims::default_usb_source)
    }

    /// `sdk` with every simulation switch this builder was given
    /// ([`Self::with_unopenable`], [`Self::with_departure_file`]): what a
    /// build's enumeration, identity opens and registered cameras all go
    /// through.
    #[cfg(feature = "simulation")]
    fn simulated(&self, sdk: zwo_rs::Sdk) -> zwo_rs::Sdk {
        let sdk = if self.unopenable {
            sdk.with_unopenable()
        } else {
            sdk
        };
        match &self.departure_file {
            Some(path) => sdk.with_departure_file(path),
            None => sdk,
        }
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
    /// Returns [`ZwoCameraError`] when the list breaks a rule, SDK
    /// enumeration fails — or, with no list, a camera cannot be opened for its
    /// serial — or the listener cannot bind the configured port. A failed USB
    /// scan is not an error: it is served as placeholders and retried (U6).
    pub async fn build(self) -> Result<BoundServer, ZwoCameraError> {
        // The load refuses a list that breaks a rule; checked again here
        // because registration depends on it — numbers 0..N-1 are what make
        // each entry's position its number — and a config built in code
        // never passed through a load.
        self.config.check_list().map_err(ZwoCameraError::Config)?;
        let mut server = Server::new(CargoServerInfo!());
        let held = HeldCameras::default();
        let registered = match &self.config.usb_devices {
            None => Registered {
                cameras: self.register_enumerated(&mut server, &held).await?,
                placeholders: 0,
                rescan: None,
            },
            Some(list) => self.register_list(&mut server, &held, list).await?,
        };

        // Use the shared dual-stack helper (IPv6 + IPv4) with SO_REUSEADDR, like
        // every other Alpaca service. SO_REUSEADDR matters here because the
        // in-process `with_reload` loop rebinds the same port; a raw bind could
        // fail to rebind while a prior listener's TIME_WAIT lingers.
        let bind_addr = self.config.server.socket_addr();
        let listener = rusty_photon_tls::server::bind_dual_stack_tokio(bind_addr)
            .await
            .map_err(|source| ZwoCameraError::Bind {
                addr: bind_addr.to_string(),
                source,
            })?;
        let local_addr = listener
            .local_addr()
            .map_err(|source| ZwoCameraError::Bind {
                addr: bind_addr.to_string(),
                source: rusty_photon_tls::error::TlsError::Io(source),
            })?;

        // Opt-in Alpaca UDP discovery responder (config `discovery_port`);
        // bound here so a taken port fails startup, run in start().
        let discovery =
            rusty_photon_driver::discovery::bind(local_addr, self.config.server.discovery_port)
                .await
                .map_err(|e| ZwoCameraError::Discovery(e.to_string()))?;

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
        // discovery (see rusty-photon-service-lifecycle's logging docs); every
        // peer service emits it. Logs go to stderr, so this stays parseable.
        // Console mode only: stdout is a dead handle under the Windows SCM,
        // and the only stdout consumer (bdd-infra's port parser) never runs
        // services with --service.
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

    /// The no-list default: every camera the SDK enumerates, in SDK order,
    /// each opened briefly for its serial. Returns how many were registered.
    async fn register_enumerated(
        &self,
        server: &mut Server,
        held: &HeldCameras,
    ) -> Result<usize, ZwoCameraError> {
        let cameras = if self.force_empty {
            Vec::new()
        } else {
            let sdk = zwo_rs::Sdk::new()?;
            #[cfg(feature = "simulation")]
            let sdk = self.simulated(sdk);
            enumerate_cameras(sdk).await?
        };
        if cameras.is_empty() {
            warn!("no ASI cameras discovered; starting with no Camera devices");
        }
        for cam in &cameras {
            // `devices` overrides are keyed by the bare SDK serial (matching the
            // config-actions `devices.{serial}` paths), NOT the prefixed
            // `ZWO:{name}:{serial}` UniqueID — looking up by UniqueID would never
            // match a user's serial-keyed entry.
            self.register_camera(server, held, cam, self.config.devices.get(&cam.serial))?;
        }
        Ok(cameras.len())
    }

    /// A `usb_devices` list: take the USB scan, enumerate the SDK, place each
    /// SDK camera on a port, open each camera placed on a listed port —
    /// uninitialised, for its serial — and register each entry in number
    /// order: the camera placed on its port (U5), or a placeholder that says
    /// why there is none (U4). A camera on an unlisted port is never opened.
    /// Returns what was registered, and the re-scan to run when the scan
    /// failed (U6).
    async fn register_list(
        &self,
        server: &mut Server,
        held: &HeldCameras,
        list: &[UsbDeviceEntry],
    ) -> Result<Registered, ZwoCameraError> {
        if list.is_empty() {
            warn!(
                "usb_devices is an empty list: no ASI camera is registered, every one is left \
                 to other applications"
            );
            return Ok(Registered::default());
        }
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
        }
        let mut entries = list.to_vec();
        entries.sort_by_key(|e| e.device_number);
        let sdk = zwo_rs::Sdk::new()?;
        #[cfg(feature = "simulation")]
        let sdk = self.simulated(sdk);
        let force_empty = self.force_empty;
        let placed =
            tokio::task::spawn_blocking(move || place_list(&sdk, scan, entries, force_empty))
                .await??;

        let mut registered = Registered::default();
        for (entry, outcome) in &placed.entries {
            match outcome {
                Ok(cam) => {
                    debug!(
                        device = entry.device_number,
                        port = %entry.usb_port,
                        "a listed port holds an ASI camera"
                    );
                    self.register_camera(server, held, cam, Some(&entry.display_override()))?;
                    registered.cameras = registered.cameras.saturating_add(1);
                }
                Err(reason) => {
                    warn!(
                        device = entry.device_number,
                        port = %entry.usb_port,
                        %reason,
                        "a listed number is held by a placeholder"
                    );
                    let mut placeholder = claims::placeholder(entry, reason.clone());
                    if let Some(ctx) = self.config_actions() {
                        placeholder = placeholder.with_config_actions(ctx);
                    }
                    server.devices.register(placeholder);
                    registered.placeholders = registered.placeholders.saturating_add(1);
                }
            }
        }
        for (index, port) in &placed.unlisted {
            debug!(
                camera = index,
                port, "an ASI camera on an unlisted port is neither opened nor served"
            );
        }
        if placed.scan_failed {
            registered.rescan = Some(self.rescan());
        }
        Ok(registered)
    }

    /// Register one enumerated camera with its display overrides.
    fn register_camera(
        &self,
        server: &mut Server,
        held: &HeldCameras,
        cam: &EnumeratedCamera,
        overrides: Option<&DeviceOverride>,
    ) -> Result<(), ZwoCameraError> {
        let sdk = zwo_rs::Sdk::new()?;
        #[cfg(feature = "simulation")]
        let sdk = self.simulated(sdk);
        let handle: Arc<dyn CameraHandle> = Arc::new(ZwoCameraHandle::new(
            sdk,
            cam.index,
            cam.info.clone(),
            cam.unique_id.clone(),
            cam.hardware_serial.clone(),
            Arc::clone(held),
        ));
        let mut device = ZwoCamera::new(handle, overrides, self.config.max_adu_reporting);
        if let Some(ctx) = self.config_actions() {
            device = device.with_config_actions(ctx);
        }
        debug!(sdk_index = cam.index, name = %device.static_name(), "registering ASI camera");
        server.devices.register(device);
        Ok(())
    }

    /// The config-action context every registered device carries, when the
    /// builder has a config source and a reload path.
    fn config_actions(&self) -> Option<ConfigActionCtx<ZwoCameraDriver>> {
        let (path, reload) = (self.config_path.clone()?, self.reload.clone()?);
        Some(ConfigActionCtx {
            effective: self.config.clone(),
            path,
            overrides: self.overrides.clone(),
            reload,
        })
    }

    /// The background re-scan after a failed USB scan (U6): on the first try
    /// after which a rebuild would serve the list, it ends its own server,
    /// which the caller rebuilds ([`Ended::Rescanned`]) — placing the listed
    /// cameras afresh.
    fn rescan(&self) -> Rescan {
        let source = self.usb_source();
        let config = self
            .config_path
            .clone()
            .map(|path| (path, self.overrides.clone()));
        Rescan::new(claims::SDK_NAME, move || {
            rescan_blocker(config.as_ref(), &source)
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

/// What a `usb_devices` list resolved to at one start or reload.
struct PlacedList {
    /// Each entry, in number order, with its camera or the reason a
    /// placeholder holds its number.
    entries: Vec<(UsbDeviceEntry, Result<EnumeratedCamera, String>)>,
    /// The SDK cameras placed on ports no entry lists, with their ports.
    unlisted: Vec<(usize, String)>,
    /// The USB scan could not run.
    scan_failed: bool,
}

/// Enumerate the SDK, place each camera on a port, and open each camera
/// placed on a listed port for its identity — all under one hold of the SDK's
/// camera list, so no rescan can renumber it between the index the join chose
/// and the camera opened (U1). A failed scan enumerates nothing; so does
/// `force_empty`. Blocking.
fn place_list(
    sdk: &zwo_rs::Sdk,
    scan: Result<UsbScan, String>,
    entries: Vec<UsbDeviceEntry>,
    force_empty: bool,
) -> Result<PlacedList, zwo_rs::Error> {
    let camera_list = sdk.camera_list();
    let infos = if scan.is_err() || force_empty {
        Vec::new()
    } else {
        camera_list.rescan()?
    };
    let claims = Claims::new(
        claims::NORMALIZER,
        claims::SERVICE,
        scan,
        claims::sdk_cameras(&infos),
    );
    let resolved = resolve_list(&claims, &infos, entries, |index, info| {
        identity(&camera_list, index, info)
    });
    drop(camera_list);
    let unlisted = claims
        .placed()
        .into_iter()
        .filter(|(_, port)| !resolved.iter().any(|(e, _)| e.usb_port == *port))
        .map(|(index, port)| (index, port.to_string()))
        .collect();
    Ok(PlacedList {
        unlisted,
        scan_failed: claims.scan_error().is_some(),
        entries: resolved,
    })
}

/// Resolve each entry against the join: the camera placed on its port, opened
/// through `open` for its identity, or the reason a placeholder holds its
/// number — the join's, or the SDK's open error (U4, U5). `open` is called
/// for the cameras placed on listed ports and no other.
fn resolve_list(
    claims: &Claims<'_>,
    infos: &[CameraInfo],
    entries: Vec<UsbDeviceEntry>,
    mut open: impl FnMut(usize, &CameraInfo) -> Result<EnumeratedCamera, zwo_rs::Error>,
) -> Vec<(UsbDeviceEntry, Result<EnumeratedCamera, String>)> {
    entries
        .into_iter()
        .map(|entry| {
            let outcome = match claims.resolve(&entry.usb_port) {
                Resolution::Camera(index) => infos.get(index).map_or_else(
                    || {
                        Err(format!(
                            "the join placed SDK camera {index}, which is not enumerated"
                        ))
                    },
                    |info| {
                        open(index, info).map_err(|error| {
                            claims.open_failure(&entry.usb_port, &error.to_string())
                        })
                    },
                ),
                Resolution::Placeholder(reason) => Err(reason),
            };
            (entry, outcome)
        })
        .collect()
}

/// What would keep a rebuild now from serving the list (U6), in
/// [`first_blocker`]'s order, or `None` when nothing would. Blocking. `config`
/// is the file the rebuild will read and the CLI overrides layered on it,
/// `None` when the builder has no config source.
fn rescan_blocker(config: Option<&(PathBuf, CliOverrides)>, source: &UsbSource) -> Option<Blocker> {
    first_blocker(
        || {
            config.map_or(Ok(true), |(path, overrides)| {
                load_effective_config(path, overrides)
                    .map(|config| config.usb_devices.is_some())
                    .map_err(|error| error.to_string())
            })
        },
        || source.scan().map(drop),
        || {
            zwo_rs::Sdk::new()
                .and_then(|sdk| sdk.cameras())
                .map(drop)
                .map_err(|error| error.to_string())
        },
    )
}

/// A zwo-camera server bound to a local port, ready to [`start`](Self::start).
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
    /// Returns [`ZwoCameraError::Server`] if the HTTP server stops with an error.
    pub async fn start(
        self,
        shutdown: impl Future<Output = ()> + Send + 'static,
    ) -> Result<Ended, ZwoCameraError> {
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
        let (shutdown, cause) = rusty_photon_driver::claims::stop_or_rescanned(shutdown, rescan);
        let serve = async {
            let result = if let Some(ref tls_config) = tls {
                debug!("serving over TLS");
                rusty_photon_tls::server::serve_tls(listener, app, tls_config, shutdown).await
            } else {
                debug!("serving plain HTTP");
                rusty_photon_tls::server::serve_plain(listener, app, shutdown).await
            };
            result.map_err(|e| ZwoCameraError::Server(e.to_string()))
        };
        rusty_photon_driver::discovery::serve_with(discovery, serve).await?;
        Ok(cause.ended())
    }
}

/// Enumerate connected ASI cameras on the blocking thread pool, minting each
/// device's serial-derived `UniqueID` — the no-list default (C0).
///
/// `ASIGetSerialNumber` requires an *open* camera, so each is opened briefly via
/// [`zwo_rs::CameraList::open_uninitialised`] to read its serial and then closed —
/// that call deliberately never runs `ASIInitCamera` (which resets controls, e.g.
/// the cooler, to SDK defaults), so this passive path touches no camera state (the
/// per-device connect handshake, which does initialise, happens later on
/// `set_connected(true)`; see contract C5). The ASI SDK is blocking C FFI, so
/// every SDK call funnels through [`tokio::task::spawn_blocking`] (design doc
/// "Concurrency"), and the enumeration and the opens are one hold of the SDK's
/// camera list, so no rescan renumbers it in between.
///
/// A camera that cannot even be *opened* (`open_uninitialised` itself fails —
/// e.g. removed, or wedged) fails this whole enumeration via `?`; with a
/// `usb_devices` list the same failure costs only that camera's number (U5).
/// Only a post-open [`serial`](zwo_rs::UninitialisedCamera::serial) failure (no
/// serial, no flash id) is downgraded here, to the position-based fallback.
async fn enumerate_cameras(sdk: zwo_rs::Sdk) -> Result<Vec<EnumeratedCamera>, ZwoCameraError> {
    let cameras =
        tokio::task::spawn_blocking(move || -> Result<Vec<EnumeratedCamera>, zwo_rs::Error> {
            let list = sdk.camera_list();
            let infos = list.rescan()?;
            let cameras = infos
                .iter()
                .enumerate()
                .map(|(index, info)| identity(&list, index, info))
                .collect();
            drop(list);
            cameras
        })
        .await??;
    debug!(count = cameras.len(), "enumerated ASI cameras");
    Ok(cameras)
}

/// Open the camera at `index` in `list` briefly, uninitialised, to mint its
/// identity (C0, C5): the camera is closed again when the read is done.
/// Blocking.
///
/// # Errors
/// The SDK's error when the camera cannot be opened at all. A camera that
/// opens but exposes no serial or flash id is not an error: it takes the
/// position-based fallback, logged at `warn!`.
fn identity(
    list: &zwo_rs::CameraList<'_>,
    index: usize,
    info: &CameraInfo,
) -> Result<EnumeratedCamera, zwo_rs::Error> {
    // The camera drops at the end of the block → `ASICloseCamera`.
    let serial_result = {
        let camera = list.open_uninitialised(index)?;
        camera.serial()
    };
    if let Err(ref e) = serial_result {
        warn!(
            camera = %info.name, error = %e,
            "camera exposes no hardware serial or flash ID; using a position-based identity"
        );
    }
    let hardware_serial = serial_result.as_ref().ok().cloned();
    let (serial, unique_id) = mint_identity(serial_result, &info.name, index);
    Ok(EnumeratedCamera {
        index,
        info: info.clone(),
        serial,
        hardware_serial,
        unique_id,
    })
}

/// Mint the `(serial, UniqueID)` pair for an enumerated camera.
///
/// The serial is the camera's hardware serial (`ASIGetSerialNumber`) or, failing
/// that, its programmed flash ID (`ASIGetID`). Older ASI models (e.g. the
/// ASI1600) expose **neither** and fail the read. Rather than make the whole
/// service unstartable for such a camera, fall back to a stable position-based
/// identity (`noserial-{index}`): unique per enumeration slot and stable across
/// reconnects for the common single-camera case. The only ambiguity is two
/// serial-less cameras of the same model reordered on the bus, which could swap
/// identities — an acceptable trade for the camera working at all.
fn mint_identity(
    serial: Result<String, zwo_rs::Error>,
    name: &str,
    index: usize,
) -> (String, String) {
    let serial = serial.unwrap_or_else(|_| format!("noserial-{index}"));
    let unique_id = format!("ZWO:{}:{}", name.replace(' ', "-"), serial);
    (serial, unique_id)
}

#[cfg(test)]
mod identity_tests {
    use super::mint_identity;

    #[test]
    fn mint_identity_uses_hardware_serial_when_present() {
        let (serial, unique_id) =
            mint_identity(Ok("1915d5081b090900".to_owned()), "ZWO ASI178MM", 0);
        assert_eq!(serial, "1915d5081b090900");
        assert_eq!(unique_id, "ZWO:ZWO-ASI178MM:1915d5081b090900");
    }

    #[test]
    fn mint_identity_falls_back_to_position_when_no_serial() {
        // Older models (e.g. the ASI1600) report neither a serial nor a flash ID.
        let err = Err(zwo_rs::Error::Asi(zwo_rs::AsiError::GeneralError));
        let (serial, unique_id) = mint_identity(err, "ZWO ASI1600MM-Cool", 0);
        assert_eq!(serial, "noserial-0");
        assert_eq!(unique_id, "ZWO:ZWO-ASI1600MM-Cool:noserial-0");
    }
}

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
mod list_tests {
    use super::*;
    use rusty_photon_doctor_checks::UsbDevice;

    fn info(name: &str) -> CameraInfo {
        CameraInfo {
            id: 0,
            name: name.to_string(),
            max_width: 1,
            max_height: 1,
            is_color: false,
            bayer_pattern: zwo_rs::BayerPattern::Rg,
            supported_bins: vec![1],
            supported_video_formats: Vec::new(),
            pixel_size_um: 1.0,
            has_mechanical_shutter: false,
            has_st4_port: false,
            is_cooler_cam: false,
            is_usb3: true,
            e_per_adu: 1.0,
            bit_depth: 16,
            is_trigger_cam: false,
        }
    }

    fn record(product: &str, port: &str) -> UsbDevice {
        UsbDevice {
            vendor: "03c3".to_string(),
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
        }
    }

    /// The dev box's ASI1600MM-Cool (SDK index 0) on p1 and ASI178MM (1) on
    /// p2.
    fn two_cameras() -> (Claims<'static>, Vec<CameraInfo>) {
        let infos = vec![info("ZWO ASI1600MM-Cool"), info("ZWO ASI178MM")];
        let claims = Claims::new(
            claims::NORMALIZER,
            claims::SERVICE,
            Ok(UsbScan {
                devices: vec![record("1603", "p1"), record("178c", "p2")],
                faults: Vec::new(),
            }),
            claims::sdk_cameras(&infos),
        );
        (claims, infos)
    }

    fn opened_as(index: usize, info: &CameraInfo) -> EnumeratedCamera {
        EnumeratedCamera {
            index,
            info: info.clone(),
            serial: format!("serial-{index}"),
            hardware_serial: None,
            unique_id: format!("ZWO:{}:serial-{index}", info.name),
        }
    }

    #[test]
    fn only_the_cameras_on_listed_ports_are_opened() {
        let (claims, infos) = two_cameras();
        let mut opened = Vec::new();
        let resolved = resolve_list(&claims, &infos, vec![entry(0, "p2")], |index, info| {
            opened.push(index);
            Ok(opened_as(index, info))
        });
        assert_eq!(opened, vec![1], "only the ASI178MM's port is listed");
        let (listed, outcome) = &resolved[0];
        assert_eq!(listed.usb_port, "p2");
        assert_eq!(outcome.as_ref().unwrap().index, 1);
    }

    #[test]
    fn a_port_with_no_camera_is_never_opened() {
        let (claims, infos) = two_cameras();
        let mut opened = Vec::new();
        let resolved = resolve_list(&claims, &infos, vec![entry(0, "p9")], |index, info| {
            opened.push(index);
            Ok(opened_as(index, info))
        });
        assert_eq!(opened, Vec::<usize>::new());
        let reason = resolved[0].1.as_ref().unwrap_err();
        assert!(
            reason.starts_with("no working camera is enumerated on p9."),
            "{reason}"
        );
    }

    #[test]
    fn a_camera_that_cannot_be_opened_holds_its_number_alone() {
        let (claims, infos) = two_cameras();
        let resolved = resolve_list(
            &claims,
            &infos,
            vec![entry(0, "p1"), entry(1, "p2")],
            |index, info| {
                if index == 0 {
                    Err(zwo_rs::Error::Asi(zwo_rs::AsiError::GeneralError))
                } else {
                    Ok(opened_as(index, info))
                }
            },
        );
        let reason = resolved[0].1.as_ref().unwrap_err();
        assert!(
            reason.starts_with(
                "the ZWO SDK could not open the camera placed on p1: ASI camera SDK error: \
                 general error"
            ),
            "{reason}"
        );
        assert!(reason.contains("reload or restart zwo-camera"), "{reason}");
        assert_eq!(resolved[1].1.as_ref().unwrap().index, 1);
    }

    #[test]
    fn a_failed_scan_opens_nothing() {
        let infos = vec![info("ZWO ASI1600MM-Cool")];
        let claims = Claims::new(
            claims::NORMALIZER,
            claims::SERVICE,
            Err("powershell.exe timed out".to_string()),
            claims::sdk_cameras(&infos),
        );
        let mut opened = Vec::new();
        let resolved = resolve_list(&claims, &infos, vec![entry(0, "p1")], |index, info| {
            opened.push(index);
            Ok(opened_as(index, info))
        });
        assert_eq!(opened, Vec::<usize>::new());
        let reason = resolved[0].1.as_ref().unwrap_err();
        assert!(
            reason.starts_with("the USB scan failed: powershell.exe timed out."),
            "{reason}"
        );
    }
}

#[cfg(all(test, feature = "simulation"))]
#[cfg_attr(coverage_nightly, coverage(off))]
mod simulation_tests {
    use super::*;

    /// End-to-end proof against the `zwo-rs` simulation backend: the builder
    /// enumerates the one simulated camera, registers it, and binds an ephemeral
    /// port.
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
    /// `ZWO:{name}:{serial}` `UniqueID`. The simulated camera's serial resolves an
    /// override entry; the `UniqueID` does not.
    #[tokio::test]
    async fn device_overrides_are_keyed_by_serial_not_unique_id() {
        let cam = &enumerate_cameras(zwo_rs::Sdk::new().unwrap())
            .await
            .unwrap()[0];
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
        assert!(
            config.devices.contains_key(&cam.serial),
            "the serial must resolve the override the builder applies"
        );
        assert!(
            !config.devices.contains_key(&cam.unique_id),
            "the UniqueID must NOT resolve it (the bug Copilot flagged)"
        );
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
    ) -> Result<(Vec<String>, Registered), ZwoCameraError> {
        let mut server = Server::new(CargoServerInfo!());
        let registered = builder
            .register_list(&mut server, &HeldCameras::default(), &list(entries))
            .await?;
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
                "placeholder:zwo-camera:simulated-usbv3-0:9",
                "ZWO:ASI2600MM-Pro-Simulated:0a1b2c3d4e5f6071",
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
        assert_eq!(ids, vec!["placeholder:zwo-camera:simulated-usbv3-0:1"]);
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
        assert_eq!(ids, vec!["placeholder:zwo-camera:simulated-usbv3-0:1"]);
    }

    #[tokio::test]
    async fn a_listed_camera_that_cannot_be_opened_is_a_placeholder() {
        let builder = ServerBuilder::new().with_unopenable(true);
        let (ids, registered) = register(&builder, &[(0, "simulated-usbv3-0:1")])
            .await
            .unwrap();
        assert_eq!(ids, vec!["placeholder:zwo-camera:simulated-usbv3-0:1"]);
        assert_eq!((registered.cameras, registered.placeholders), (0, 1));
    }

    #[tokio::test]
    async fn an_unlisted_camera_that_cannot_be_opened_costs_nothing() {
        let builder = ServerBuilder::new().with_unopenable(true);
        let (ids, _) = register(&builder, &[(0, "simulated-usbv3-0:9")])
            .await
            .unwrap();
        assert_eq!(ids, vec!["placeholder:zwo-camera:simulated-usbv3-0:9"]);
    }

    #[tokio::test]
    async fn without_a_list_a_camera_that_cannot_be_opened_fails_the_build() {
        let config: Config = serde_json::from_str(r#"{"server":{"port":0}}"#).unwrap();
        let error = ServerBuilder::new()
            .with_config(config)
            .with_unopenable(true)
            .build()
            .await
            .err()
            .unwrap()
            .to_string();
        assert!(error.contains("general error"), "{error}");
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

    /// A config file at a fresh path holding `json`, kept alive by the guard.
    fn config_file(json: &str) -> std::io::Result<(tempfile::TempDir, (PathBuf, CliOverrides))> {
        let dir = tempfile::tempdir()?;
        let path = dir.path().join("zwo-camera.json");
        std::fs::write(&path, json)?;
        Ok((dir, (path, CliOverrides::default())))
    }

    #[test]
    fn a_rescan_with_nothing_in_the_way_finds_no_blocker() {
        let (_dir, config) =
            config_file(r#"{"usb_devices": [{"device_number": 0, "usb_port": "a"}]}"#).unwrap();
        let blocker = rescan_blocker(Some(&config), &claims::default_usb_source());
        assert_eq!(blocker, None);
    }

    #[test]
    fn a_rescan_leaves_a_file_with_no_list_to_a_reload() {
        // A hand edit dropped the list: a rebuild would take the no-list path,
        // which nobody asked for. The scan would pass; the file blocks first.
        let (_dir, config) = config_file("{}").unwrap();
        let blocker = rescan_blocker(Some(&config), &claims::default_usb_source());
        assert_eq!(blocker, Some(Blocker::NoList));
    }

    #[test]
    fn a_rescan_is_blocked_while_the_scan_fails() {
        let (_dir, source) = staged(FAILED_SCAN).unwrap();
        assert_eq!(
            rescan_blocker(None, &source),
            Some(Blocker::Scan("powershell.exe timed out".to_string()))
        );
    }

    #[test]
    fn a_rescan_checks_the_config_file_before_it_scans() {
        // A scan of this source would fail; a config that does not load is
        // what the blocker names, so no scan was taken.
        let (_dir, config) =
            config_file(r#"{"usb_devices": [{"device_number": 1, "usb_port": "a"}]}"#).unwrap();
        let source = UsbSource::Staged(PathBuf::from("/nonexistent/inventory.json"));
        let blocker = rescan_blocker(Some(&config), &source);
        assert!(matches!(blocker, Some(Blocker::Config(_))), "{blocker:?}");
    }
}
