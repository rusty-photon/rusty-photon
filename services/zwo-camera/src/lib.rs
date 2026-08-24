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
//! The service enumerates every connected ASI camera via [`zwo_rs`] and registers
//! each as an ASCOM [`ZwoCamera`] device on one port, with a serial-derived
//! `UniqueID`. Each device implements the full `Device` + `Camera` surface
//! (exposure state machine, ROI/binning, gain/offset, cooling, readout, ST4
//! pulse-guiding) over the [`backend::CameraHandle`] SDK seam. EFW filter
//! wheels and EAF focusers are separate services (ADR-014): each independently
//! usable ZWO device gets its own driver process.
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
mod config;
mod config_actions;
pub mod doctor;
mod error;

pub use camera::ZwoCamera;
pub use config::{
    load_effective_config, AlpacaServerConfig, CliOverrides, Config, DeviceOverride, DEFAULT_PORT,
};
pub use config_actions::ZwoCameraDriver;
pub use error::ZwoCameraError;

use std::future::Future;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;

use ascom_alpaca::api::{CargoServerInfo, Device};
use ascom_alpaca::Server;
use rusty_photon_service_lifecycle::ReloadSignal;
use rusty_photon_tls::config::TlsConfig;
use tokio::net::TcpListener;
use tracing::{debug, info, warn};
use zwo_rs::CameraInfo;

use crate::backend::{CameraHandle, ZwoCameraHandle};

/// One camera discovered at enumeration: its index, [`CameraInfo`], the bare
/// SDK `serial` (the key for `devices` config overrides), and the
/// serial-derived ASCOM `UniqueID`.
struct EnumeratedCamera {
    index: usize,
    info: CameraInfo,
    serial: String,
    unique_id: String,
}

/// Builds a bound zwo-camera server from an effective [`Config`].
#[derive(Default)]
pub struct ServerBuilder {
    config: Config,
    config_path: Option<PathBuf>,
    overrides: CliOverrides,
    reload: Option<ReloadSignal>,
    /// Register no cameras (the test-only zero-camera startup path, C0).
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

    /// Enumerate the connected ASI cameras, register each as an ASCOM device,
    /// and bind the Alpaca listener.
    ///
    /// Zero discovered cameras is **not** a hard failure: the server starts with
    /// no Camera devices and logs a warning (a later reload re-enumerates).
    ///
    /// # Errors
    /// Returns [`ZwoCameraError`] when SDK enumeration fails or the listener
    /// cannot bind the configured port.
    pub async fn build(self) -> Result<BoundServer, ZwoCameraError> {
        let cameras = if self.force_empty {
            Vec::new()
        } else {
            enumerate_cameras().await?
        };
        if cameras.is_empty() {
            warn!("no ASI cameras discovered; starting with no Camera devices");
        }

        let mut server = Server::new(CargoServerInfo!());
        for cam in &cameras {
            let handle: Arc<dyn CameraHandle> = Arc::new(ZwoCameraHandle::new(
                zwo_rs::Sdk::new()?,
                cam.index,
                cam.info.clone(),
                cam.unique_id.clone(),
            ));
            // `devices` overrides are keyed by the bare SDK serial (matching the
            // config-actions `devices.{serial}` paths), NOT the prefixed
            // `ZWO:{name}:{serial}` UniqueID — looking up by UniqueID would never
            // match a user's serial-keyed entry.
            let mut device = ZwoCamera::new(
                handle,
                self.config.devices.get(&cam.serial),
                self.config.max_adu_reporting,
            );
            if let (Some(path), Some(reload)) = (self.config_path.clone(), self.reload.clone()) {
                device = device.with_config_actions(rusty_photon_driver::ConfigActionCtx {
                    effective: self.config.clone(),
                    path,
                    overrides: self.overrides.clone(),
                    reload,
                });
            }
            debug!(device = cam.index, name = %device.static_name(), "registering ASI camera");
            server.devices.register(device);
        }

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
    /// Returns [`ZwoCameraError::Server`] if the HTTP server stops with an error.
    pub async fn start(
        self,
        shutdown: impl Future<Output = ()> + Send + 'static,
    ) -> Result<(), ZwoCameraError> {
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
            result.map_err(|e| ZwoCameraError::Server(e.to_string()))
        };
        rusty_photon_driver::discovery::serve_with(discovery, serve).await?;
        Ok(())
    }
}

/// Enumerate connected ASI cameras on the blocking thread pool, minting each
/// device's serial-derived `UniqueID`.
///
/// `ASIGetSerialNumber` requires an *open* camera, so each is opened briefly via
/// [`zwo_rs::Sdk::open_uninitialised`] to read its serial and then closed — that
/// call deliberately never runs `ASIInitCamera` (which resets controls, e.g. the
/// cooler, to SDK defaults), so this passive path touches no camera state (the
/// per-device connect handshake, which does initialise, happens later on
/// `set_connected(true)`; see contract C5). The ASI SDK is blocking C FFI, so
/// every SDK call funnels through [`tokio::task::spawn_blocking`] (design doc
/// "Concurrency").
///
/// A camera that cannot even be *opened* (`open_uninitialised` itself fails —
/// e.g. removed, or claimed by another process) fails this whole enumeration
/// via `?`, same as before this method stopped also initialising: only a
/// post-open [`serial`](zwo_rs::UninitialisedCamera::serial) failure (no serial,
/// no flash id) is caught below and downgraded to the position-based fallback.
async fn enumerate_cameras() -> Result<Vec<EnumeratedCamera>, ZwoCameraError> {
    let cameras =
        tokio::task::spawn_blocking(|| -> Result<Vec<EnumeratedCamera>, zwo_rs::Error> {
            let sdk = zwo_rs::Sdk::new()?;
            let infos = sdk.cameras()?;
            let mut out = Vec::with_capacity(infos.len());
            for (index, info) in infos.into_iter().enumerate() {
                // Open briefly to read the stable serial, then close (the camera
                // drops at the end of the block → `ASICloseCamera`).
                let serial_result = {
                    let camera = sdk.open_uninitialised(index)?;
                    camera.serial()
                };
                if let Err(ref e) = serial_result {
                    warn!(
                        camera = %info.name, error = %e,
                        "camera exposes no hardware serial or flash ID; using a position-based identity"
                    );
                }
                let (serial, unique_id) = mint_identity(serial_result, &info.name, index);
                out.push(EnumeratedCamera {
                    index,
                    info,
                    serial,
                    unique_id,
                });
            }
            Ok(out)
        })
        .await??;
    debug!(count = cameras.len(), "enumerated ASI cameras");
    Ok(cameras)
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
        let cam = &enumerate_cameras().await.unwrap()[0];
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
}
