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
//! `build()` enumerates whatever `svbony_rs::Sdk::cameras()` reports and
//! registers each camera as an ASCOM device. With the `simulation` feature
//! that is `svbony-rs`'s one fabricated `SV605CC-Simulated` camera (so BDD
//! scenarios have "camera device 0" to address); the production real-SDK
//! build registers the physically connected cameras.

// Curated test-scope allow list — documented in the root Cargo.toml [workspace.lints] block.
#![cfg_attr(
    test,
    allow(
        clippy::needless_pass_by_ref_mut,
        clippy::needless_pass_by_value,
        clippy::unused_async,
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
mod config;
mod config_actions;
pub mod doctor;
mod error;

pub use camera::SvbonyCamera;
pub use config::{
    load_effective_config, AlpacaServerConfig, CliOverrides, Config, DeviceOverride, DEFAULT_PORT,
};
pub use config_actions::SvbonyCameraDriver;
pub use error::SvbonyCameraError;

use std::future::Future;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;

use ascom_alpaca::api::{CargoServerInfo, Device};
use ascom_alpaca::Server;
use rusty_photon_service_lifecycle::ReloadSignal;
use rusty_photon_tls::config::TlsConfig;
use svbony_rs::CameraInfo;
use tokio::net::TcpListener;
use tracing::{debug, info, warn};

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
    /// `zwo-camera`'s `--simulation-empty` (contract C0).
    force_empty: bool,
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

    /// Enumerate the connected `SVBony` cameras, register each as an ASCOM
    /// device, and bind the Alpaca listener.
    ///
    /// Zero discovered cameras is **not** a hard failure: the server starts
    /// with no Camera devices and logs a warning (a later reload
    /// re-enumerates). See [`enumerate_cameras`] for this phase's
    /// simulation-only registration boundary.
    ///
    /// # Errors
    /// Returns [`SvbonyCameraError`] when SDK enumeration fails or the
    /// listener cannot bind the configured port.
    pub async fn build(self) -> Result<BoundServer, SvbonyCameraError> {
        let cameras = if self.force_empty {
            Vec::new()
        } else {
            enumerate_cameras().await?
        };
        if cameras.is_empty() {
            warn!("no SVBony cameras registered; starting with no Camera devices");
        }

        let mut server = Server::new(CargoServerInfo!());
        for cam in &cameras {
            let handle: Arc<dyn CameraHandle> = Arc::new(SvbonyCameraHandle::new(
                svbony_rs::Sdk::new()?,
                cam.index,
                cam.info.clone(),
                cam.unique_id.clone(),
            ));
            // `devices` overrides are keyed by the bare SDK serial (matching
            // the config-actions `devices.{serial}` paths), NOT the prefixed
            // `SVBONY:{name}:{serial}` UniqueID.
            let mut device = SvbonyCamera::new(handle, self.config.devices.get(&cam.serial));
            if let (Some(path), Some(reload)) = (self.config_path.clone(), self.reload.clone()) {
                device = device.with_config_actions(rusty_photon_driver::ConfigActionCtx {
                    effective: self.config.clone(),
                    path,
                    overrides: self.overrides.clone(),
                    reload,
                });
            }
            debug!(device = cam.index, name = %device.static_name(), "registering SVBony camera");
            server.devices.register(device);
        }

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
        info!(cameras = cameras.len(), address = %local_addr, "Service started successfully");
        Ok(BoundServer {
            listener,
            app,
            local_addr,
            tls,
            discovery,
        })
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
        } = self;
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
    let serial = if info.serial.is_empty() {
        warn!(
            camera = %info.friendly_name,
            "camera reports an empty serial; using a position-based identity"
        );
        format!("noserial-{index}")
    } else {
        info.serial.clone()
    };
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
