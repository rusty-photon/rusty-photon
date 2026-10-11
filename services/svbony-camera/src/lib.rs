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
    load_effective_config, AlpacaServerConfig, CliOverrides, Config, DeviceOverride,
    UsbDeviceEntry, DEFAULT_PORT,
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
        let mut server = Server::new(CargoServerInfo!());
        let (registered, rescan) = match &self.config.usb_devices {
            None => (self.register_enumerated(&mut server).await?, None),
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
        info!(cameras = registered, address = %local_addr, "Service started successfully");
        Ok(BoundServer {
            listener,
            app,
            local_addr,
            tls,
            discovery,
            rescan,
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
    /// is none (U4). Nothing is opened. Returns how many devices were
    /// registered, and the re-scan to run when the scan failed (U6).
    async fn register_list(
        &self,
        server: &mut Server,
        list: &[UsbDeviceEntry],
    ) -> Result<(usize, Option<Rescan>), SvbonyCameraError> {
        if list.is_empty() {
            warn!(
                "usb_devices is an empty list: no SVBony camera is registered, every one is \
                 left to other applications"
            );
            return Ok((0, None));
        }
        let source = self.usb_source.clone();
        let scan = tokio::task::spawn_blocking(move || source.scan()).await?;
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
        let scan_failed = scan.is_err();
        let infos: Vec<CameraInfo> = cameras.iter().map(|c| c.info.clone()).collect();
        let claims = Claims::new(
            claims::NORMALIZER,
            claims::SERVICE,
            scan,
            claims::sdk_cameras(&infos),
        );

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

        let rescan = scan_failed
            .then(|| {
                let rescan = self.reload.clone().map(|reload| Rescan {
                    source: self.usb_source.clone(),
                    reload,
                });
                if rescan.is_none() {
                    debug!("no reload path: a failed USB scan is not retried");
                }
                rescan
            })
            .flatten();
        Ok((list.len(), rescan))
    }

    /// Register one enumerated camera with its display overrides.
    fn register_camera(
        &self,
        server: &mut Server,
        cam: &EnumeratedCamera,
        overrides: Option<&DeviceOverride>,
    ) -> Result<(), SvbonyCameraError> {
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

/// The background re-scan after a failed USB scan (U6): it waits 10 s, 20 s,
/// 40 s and then 60 s between scans, and on the first that succeeds fires the
/// service's own reload, which places the listed cameras afresh.
struct Rescan {
    source: UsbSource,
    reload: ReloadSignal,
}

impl Rescan {
    async fn run(self) {
        for wait in rusty_photon_doctor_checks::claims::rescan_waits() {
            tokio::time::sleep(wait).await;
            let source = self.source.clone();
            match tokio::task::spawn_blocking(move || source.scan()).await {
                Ok(Ok(_)) => {
                    info!("the USB scan succeeds again; reloading to place the listed cameras");
                    self.reload.notify();
                    return;
                }
                Ok(Err(error)) => debug!(%error, "the USB scan still fails"),
                Err(error) => debug!(%error, "the USB re-scan task failed"),
            }
        }
    }
}

/// Aborts the task it holds when dropped, so a re-scan ends with the server
/// it belongs to.
struct AbortOnDrop(tokio::task::JoinHandle<()>);

impl Drop for AbortOnDrop {
    fn drop(&mut self) {
        self.0.abort();
    }
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

    /// Serve until `shutdown` resolves, then drain gracefully.
    ///
    /// # Errors
    /// Returns [`SvbonyCameraError::Server`] if the HTTP server stops with an error.
    pub async fn start(
        self,
        shutdown: impl Future<Output = ()> + Send + 'static,
    ) -> Result<(), SvbonyCameraError> {
        let Self {
            listener,
            app,
            local_addr: _,
            tls,
            discovery,
            rescan,
        } = self;
        // Spawned rather than raced against serving: the re-scan's success
        // fires the reload that ends this server, and the guard stops it when
        // serving ends for any other reason.
        let _rescan = rescan.map(|rescan| AbortOnDrop(tokio::spawn(rescan.run())));
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
        Ok(())
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
    let sdk = svbony_rs::Sdk::new()?;
    let infos = sdk.cameras()?;
    Ok(infos
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

/// Mint the `(serial, UniqueID)` pair for an enumerated camera.
///
/// The serial is the camera's hardware `CameraSN`, read pre-open at
/// enumeration (unlike ZWO, no open-to-mint-identity dance is needed — see
/// [`enumerate_cameras`]'s doc comment). A camera reporting an empty serial
/// falls back to a stable position-based identity (`noserial-{index}`),
/// mirroring `zwo-camera`'s `mint_identity` fallback.
fn mint_identity(info: &CameraInfo, index: usize) -> (String, String) {
    if info.serial.is_empty() {
        warn!(
            camera = %info.friendly_name,
            "camera reports an empty serial; using a position-based identity"
        );
    }
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
    /// number order, with the re-scan the build asked for.
    async fn register(
        builder: &ServerBuilder,
        entries: &[(u32, &str)],
    ) -> Result<(Vec<String>, Option<Rescan>), SvbonyCameraError> {
        let mut server = Server::new(CargoServerInfo!());
        let (count, rescan) = builder.register_list(&mut server, &list(entries)).await?;
        let ids: Vec<String> = server
            .devices
            .iter::<dyn ascom_alpaca::api::Camera>()
            .map(|camera| camera.unique_id().to_string())
            .collect();
        assert_eq!(ids.len(), count);
        Ok((ids, rescan))
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
        let (ids, rescan) = register(
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
        assert!(rescan.is_none(), "a scan that ran needs no re-scan");
    }

    #[tokio::test]
    async fn an_empty_list_registers_nothing_and_scans_nothing() {
        // A scan of this source would fail; an empty list never takes one.
        let builder = ServerBuilder::new()
            .with_usb_source(UsbSource::Staged(PathBuf::from(
                "/nonexistent/inventory.json",
            )))
            .with_reload_signal(ReloadSignal::new());
        let (ids, rescan) = register(&builder, &[]).await.unwrap();
        assert_eq!(ids, Vec::<String>::new());
        assert!(rescan.is_none());
    }

    #[tokio::test]
    async fn a_failed_scan_holds_every_number_and_is_retried() {
        let (_dir, source) = staged(FAILED_SCAN).unwrap();
        let builder = ServerBuilder::new()
            .with_usb_source(source)
            .with_reload_signal(ReloadSignal::new());
        let (ids, rescan) = register(&builder, &[(0, "simulated-usbv3-0:1")])
            .await
            .unwrap();
        assert_eq!(ids, vec!["placeholder:svbony-camera:simulated-usbv3-0:1"]);
        assert!(rescan.is_some());
    }

    #[tokio::test]
    async fn without_a_reload_path_a_failed_scan_is_not_retried() {
        let (_dir, source) = staged(FAILED_SCAN).unwrap();
        let builder = ServerBuilder::new().with_usb_source(source);
        let (_, rescan) = register(&builder, &[(0, "simulated-usbv3-0:1")])
            .await
            .unwrap();
        assert!(rescan.is_none());
    }

    #[tokio::test]
    async fn an_empty_backend_leaves_a_listed_record_unplaced() {
        let builder = ServerBuilder::new().with_empty(true);
        let (ids, _) = register(&builder, &[(0, "simulated-usbv3-0:1")])
            .await
            .unwrap();
        assert_eq!(ids, vec!["placeholder:svbony-camera:simulated-usbv3-0:1"]);
    }

    #[tokio::test(start_paused = true)]
    async fn a_rescan_that_succeeds_fires_the_reload() {
        let reload = ReloadSignal::new();
        Rescan {
            source: UsbSource::Simulated,
            reload: reload.clone(),
        }
        .run()
        .await;
        tokio::time::timeout(std::time::Duration::ZERO, reload.recv())
            .await
            .unwrap();
    }

    #[tokio::test(start_paused = true)]
    async fn a_rescan_never_reloads_while_the_scan_fails() {
        let (_dir, source) = staged(FAILED_SCAN).unwrap();
        let reload = ReloadSignal::new();
        let run = Rescan {
            source,
            reload: reload.clone(),
        }
        .run();
        assert!(
            tokio::time::timeout(std::time::Duration::from_secs(300), run)
                .await
                .is_err(),
            "the re-scan stopped while the scan still fails"
        );
        assert!(
            tokio::time::timeout(std::time::Duration::ZERO, reload.recv())
                .await
                .is_err(),
            "a failing scan fired the reload"
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
