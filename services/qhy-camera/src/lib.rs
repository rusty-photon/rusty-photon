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
//! The service enumerates every connected QHY camera (and any CFW discovered on
//! it) at startup and registers each as an ASCOM device on one port, with a
//! serial-derived `UniqueID`. See `docs/services/qhy-camera.md`.
//!
//! **Native dependency:** `qhyccd-rs → libqhyccd-sys` links the proprietary QHYCCD
//! SDK (`static=qhyccd`) + `libusb-1.0`, so this package does not build without
//! the SDK installed. The `simulation` feature makes the backend hardware-free
//! (`Sdk::new()` fabricates a QHY178M-Simulated camera) but still links the SDK.

pub mod backend;
pub mod camera;
pub mod config;
pub mod config_actions;
pub mod doctor;
pub mod error;
pub mod filterwheel;
pub mod preflight;

pub use camera::QhyCameraDevice;
pub use config::{load_effective_config, AlpacaServerConfig, CliOverrides, Config, DeviceOverride};
pub use config_actions::QhyCameraDriver;
pub use error::{QhyCameraError, Result};
pub use filterwheel::QhyFilterWheelDevice;

use std::future::Future;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;

use ascom_alpaca::api::CargoServerInfo;
use ascom_alpaca::Server;
use rusty_photon_service_lifecycle::ReloadSignal;
use rusty_photon_tls::config::TlsConfig;
use tracing::{debug, info, warn};

use crate::backend::{
    CameraHandle, FilterWheelHandle, QhyCameraHandle, QhyFilterWheelHandle, SharedCameraConnection,
};

/// Builder for the qhy-camera ASCOM Alpaca server.
///
/// On [`build`](ServerBuilder::build) it opens the SDK, enumerates every camera
/// (and CFW when enabled), registers them as ASCOM devices, and binds the HTTP
/// listener — returning a [`BoundServer`] that keeps the SDK alive for the serve.
#[derive(Default)]
pub struct ServerBuilder {
    config: Config,
    config_path: Option<PathBuf>,
    overrides: CliOverrides,
    reload: Option<ReloadSignal>,
    /// SDK injected by `main` (the only use is the test-only empty-simulation
    /// backend); when `None`, `build()` opens the SDK via `Sdk::new()`.
    injected_sdk: Option<qhyccd_rs::Sdk>,
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

    /// Inject a pre-constructed SDK (used to select the empty simulation backend).
    #[must_use]
    pub fn with_sdk(mut self, sdk: qhyccd_rs::Sdk) -> Self {
        self.injected_sdk = Some(sdk);
        self
    }

    pub async fn build(
        self,
    ) -> std::result::Result<BoundServer, Box<dyn std::error::Error + Send + Sync>> {
        // Eager SDK open + enumerate: a discovered camera's per-device connect
        // handshake happens later on `set_connected(true)`, but the device list is
        // fixed at build (so a reload re-enumerates). Zero cameras is not a hard
        // failure (C0).
        let sdk = match self.injected_sdk {
            Some(sdk) => sdk,
            None => qhyccd_rs::Sdk::new().map_err(|e| QhyCameraError::Sdk(format!("{e:#}")))?,
        };

        let mut server = Server::new(CargoServerInfo!());
        server.listen_addr = self.config.server.socket_addr();

        // Clone the camera/CFW handles out so the `sdk` borrow ends before `sdk`
        // is moved into the BoundServer (the cloned handles share its backend).
        // Ids of cameras that report a filter wheel (each CFW wraps a Camera with
        // the same SDK id). Every discovered CFW is registered — detection is the
        // source of truth, exactly as for cameras (no opt-in toggle). Used to pair
        // a camera with its CFW so both ASCOM devices share ONE physical
        // connection — disconnecting either device must not tear down the other's
        // handle (see SharedCameraConnection).
        let cfw_ids: std::collections::HashSet<String> =
            sdk.filter_wheels().map(|w| w.id().to_string()).collect();

        let cameras: Vec<qhyccd_rs::Camera> = sdk.cameras().cloned().collect();
        let mut camera_count = 0usize;
        let mut fw_count = 0usize;
        for camera in cameras {
            let id = camera.id().to_string();
            // One shared physical connection per camera; the Camera device and (if
            // present) the CFW device both hold it and refcount open/close.
            let conn = SharedCameraConnection::new(camera);

            let handle: Arc<dyn CameraHandle> = Arc::new(QhyCameraHandle::new(conn.clone()));
            let mut device = QhyCameraDevice::new(handle, self.config.devices.get(&id));
            if let (Some(path), Some(reload)) = (self.config_path.clone(), self.reload.clone()) {
                device = device.with_config_actions(rusty_photon_driver::ConfigActionCtx {
                    effective: self.config.clone(),
                    path,
                    overrides: self.overrides.clone(),
                    reload,
                });
            }
            server.devices.register(device);
            camera_count = camera_count.saturating_add(1);
            debug!(camera = %id, "registered Camera device");

            if cfw_ids.contains(&id) {
                let handle: Arc<dyn FilterWheelHandle> =
                    Arc::new(QhyFilterWheelHandle::new(conn.clone()));
                let override_ = self.config.devices.get(&id);
                // The CFW shares the camera's SDK id, so the per-serial override's
                // `name`/`description` belong to the camera, not the wheel — only
                // `filter_names` applies here. The wheel keeps its derived name.
                let device = QhyFilterWheelDevice::new(
                    handle,
                    override_.and_then(|d| d.filter_names.clone()),
                    None,
                );
                server.devices.register(device);
                fw_count = fw_count.saturating_add(1);
                debug!(filter_wheel = %id, "registered FilterWheel device");
            }
        }
        if camera_count == 0 {
            warn!("no QHY cameras discovered; starting with no Camera devices");
        }

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
        // `with_reload` loop rebinds the same port; a raw bind could fail to
        // rebind while a prior listener's TIME_WAIT lingers.
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
            cameras = camera_count,
            filter_wheels = fw_count,
            "Service started successfully on {local_addr}"
        );

        Ok(BoundServer {
            listener,
            router,
            local_addr,
            tls,
            discovery,
            _sdk: sdk,
        })
    }
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
    /// Held so the SDK is released (its `Drop` calls `ReleaseQHYCCDResource` on
    /// real hardware) only after the HTTP server stops — on shutdown *and* reload.
    _sdk: qhyccd_rs::Sdk,
}

impl BoundServer {
    pub const fn listen_addr(&self) -> SocketAddr {
        self.local_addr
    }

    pub async fn start(
        self,
        shutdown: impl Future<Output = ()> + Send + 'static,
    ) -> std::result::Result<(), Box<dyn std::error::Error + Send + Sync>> {
        let Self {
            listener,
            router,
            local_addr: _,
            tls,
            discovery,
            _sdk,
        } = self;
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
        // `_sdk` drops here, after serving completes.
        result.map_err(Into::into)
    }
}
