//! HTTP service mode (`phd2-guider serve`) — the rp-managed guider
//! service, and its opt-in ASCOM Alpaca Camera facade.
//!
//! See `docs/services/phd2-guider.md` § "HTTP Service Mode" and
//! § "Camera facade" for the behavior contract, and
//! `docs/services/rp.md` § "Guider Service" for how `rp` consumes it.

pub mod api;
pub mod camera;
pub mod capture;
pub mod error;
pub mod guider;
pub mod lease;

pub use camera::Phd2Camera;
pub use error::{ErrorCode, ErrorResponse, ServiceError};
pub use guider::GuiderOps;

use std::future::Future;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;

use ascom_alpaca::api::CargoServerInfo;
use rusty_photon_driver::ConfigActionCtx;
use tokio::net::TcpListener;

use crate::client::Phd2Client;
use crate::config::Config;

use self::capture::CaptureEngine;

/// Two-phase server builder. `build()` binds the TCP listener (so the
/// bound port is known up-front), then `start()` serves.
///
/// Mirrors `services/plate-solver`'s `ServerBuilder` and avoids the
/// port-TOCTOU race noted in `docs/skills/development-workflow.md`
/// § "Phase 4 Stabilization".
pub struct ServerBuilder {
    config: Option<Config>,
    config_path: Option<PathBuf>,
    client: Option<Arc<Phd2Client>>,
}

impl ServerBuilder {
    #[must_use]
    pub const fn new() -> Self {
        Self {
            config: None,
            config_path: None,
            client: None,
        }
    }

    #[must_use]
    pub fn with_config(mut self, config: Config) -> Self {
        self.config = Some(config);
        self
    }

    /// The file the config came from. The camera facade's
    /// `config.apply` persists there; without it the facade serves no
    /// config actions.
    #[must_use]
    pub fn with_config_path(mut self, path: PathBuf) -> Self {
        self.config_path = Some(path);
        self
    }

    /// Override the PHD2 client (tests inject one wired to a mock
    /// connection factory; production builds one from `config.phd2`).
    #[must_use]
    pub fn with_client(mut self, client: Arc<Phd2Client>) -> Self {
        self.client = Some(client);
        self
    }

    /// Bind the listener — and, with `camera.enabled`, the camera
    /// facade's — and assemble the routers, spawning the event pump and
    /// the PHD2 connect-retry loop only after every bind succeeds.
    ///
    /// # Errors
    ///
    /// Fails when no config was supplied to the builder, when binding a
    /// listener or reading back its local address fails, or, with
    /// `camera.enabled`, when `camera.image_dir` is unusable or the
    /// camera has no `unique_id`. A PHD2 that is not yet reachable is not
    /// an error — the retry task keeps dialing it.
    pub async fn build(self) -> Result<BoundServer, std::io::Error> {
        let config = self.config.ok_or_else(|| {
            std::io::Error::other(
                "ServerBuilder::build: config is required \u{2014} call .with_config(...) first",
            )
        })?;

        let client = self
            .client
            .unwrap_or_else(|| Arc::new(Phd2Client::new(config.phd2.clone())));

        let ops = Arc::new(GuiderOps::new(
            Arc::clone(&client),
            config.settling.clone(),
            config.stop_timeout,
        ));

        // Bind before spawning any background task: a bind failure a
        // caller chooses to handle must not leak an event pump or a
        // PHD2 connect-retry loop.
        let listener = TcpListener::bind(config.server.socket_addr()).await?;
        let local_addr = listener.local_addr()?;
        let camera = if config.camera.enabled {
            Some(bind_camera(&config, self.config_path, &client, &ops).await?)
        } else {
            None
        };

        ops.spawn_event_pump();
        if let Some(camera) = &camera {
            camera.engine.recover().await;
            camera.engine.spawn_late_file_cleaner();
        }
        // A failed initial connect is not fatal: PHD2 may start later.
        // The retry task establishes the first connection; the
        // client's auto-reconnect owns recovery after that.
        ops.spawn_connect_retry(config.phd2.reconnect.interval);

        let router = api::build_router(ops);

        // Opt-in HTTP Basic Auth (shared server config `auth` block).
        let router = match &config.server.auth {
            Some(auth) => {
                warn_auth_without_tls(config.server.tls.is_none());
                rp_auth::layer(router, auth)
            }
            None => router,
        };

        Ok(BoundServer {
            listener,
            router,
            local_addr,
            tls: config.server.tls.clone(),
            camera,
        })
    }
}

impl Default for ServerBuilder {
    fn default() -> Self {
        Self::new()
    }
}

fn warn_auth_without_tls(no_tls: bool) {
    if no_tls {
        tracing::warn!(
            "Authentication is enabled but TLS is not. \
             Credentials will be transmitted in cleartext. \
             Consider enabling TLS (see `doctor --fix`)."
        );
    }
}

/// Check the facade's directory, build its device and bind its
/// loopback listener.
async fn bind_camera(
    config: &Config,
    config_path: Option<PathBuf>,
    client: &Arc<Phd2Client>,
    ops: &Arc<GuiderOps>,
) -> Result<BoundCamera, std::io::Error> {
    let camera_config = &config.camera;
    // `load_config` already refused an enabled block without these.
    let missing = |field: &str| std::io::Error::other(format!("camera.{field} is required"));
    let image_dir = camera_config
        .image_dir
        .clone()
        .ok_or_else(|| missing("image_dir"))?;
    let pixel_x = camera_config
        .pixel_size_x_um
        .ok_or_else(|| missing("pixel_size_x_um"))?;
    let pixel_y = camera_config
        .pixel_size_y_um
        .ok_or_else(|| missing("pixel_size_y_um"))?;
    let unique_id = camera_config
        .unique_id
        .clone()
        .filter(|id| !id.trim().is_empty())
        .ok_or_else(|| {
            std::io::Error::other(
                "camera.unique_id is missing; it is minted into the config file on the first \
                 start with camera.enabled, so start the service with its config file",
            )
        })?;
    // PHD2 takes the frame's path as a JSON string.
    if image_dir.to_str().is_none() {
        return Err(std::io::Error::other(format!(
            "camera.image_dir {} is not valid UTF-8",
            image_dir.display()
        )));
    }
    capture::check_image_dir(&image_dir)
        .map_err(|e| std::io::Error::other(format!("camera.image_dir {e}")))?;

    let engine = Arc::new(CaptureEngine::new(
        Arc::clone(ops),
        Arc::clone(client),
        image_dir,
        camera_config.capture_grace.duration(),
    ));
    let device = Phd2Camera::new(
        Arc::clone(client),
        Arc::clone(&engine),
        unique_id,
        camera::PixelSize {
            x_um: pixel_x.um(),
            y_um: pixel_y.um(),
        },
    );
    let device = match config_path {
        Some(path) => device.with_config_actions(ConfigActionCtx {
            effective: config.clone(),
            path,
            overrides: (),
            // Never fired: the disposition is `Restart`.
            reload: rusty_photon_service_lifecycle::ReloadSignal::new(),
        }),
        None => device,
    };

    let mut server = ascom_alpaca::Server::new(CargoServerInfo!());
    let addr = camera_config.server.socket_addr();
    server.listen_addr = addr;
    server.devices.register(device);
    let router = axum::Router::new().fallback_service(server.into_service());
    let router = match &camera_config.server.auth {
        Some(auth) => {
            warn_auth_without_tls(camera_config.server.tls.is_none());
            rp_auth::layer(router, auth)
        }
        None => router,
    };
    // The bind every other Alpaca listener uses (SO_REUSEADDR on Unix, so
    // a restart rebinds at once).
    let listener = rusty_photon_tls::server::bind_dual_stack_tokio(addr)
        .await
        .map_err(|e| std::io::Error::other(format!("camera facade cannot bind {addr}: {e}")))?;
    let local_addr = listener.local_addr()?;
    Ok(BoundCamera {
        listener,
        router,
        local_addr,
        tls: camera_config.server.tls.clone(),
        engine,
    })
}

/// The camera facade's bound listener.
struct BoundCamera {
    listener: TcpListener,
    router: axum::Router,
    local_addr: SocketAddr,
    tls: Option<rusty_photon_tls::config::TlsConfig>,
    engine: Arc<CaptureEngine>,
}

/// A fully bound server ready to accept connections.
pub struct BoundServer {
    listener: TcpListener,
    router: axum::Router,
    local_addr: SocketAddr,
    /// TLS settings from the shared server config; `None` serves plain HTTP.
    tls: Option<rusty_photon_tls::config::TlsConfig>,
    /// The camera facade, with `camera.enabled`.
    camera: Option<BoundCamera>,
}

impl BoundServer {
    pub const fn listen_addr(&self) -> SocketAddr {
        self.local_addr
    }

    /// The camera facade's address, with `camera.enabled`.
    pub fn camera_addr(&self) -> Option<SocketAddr> {
        self.camera.as_ref().map(|camera| camera.local_addr)
    }

    /// Run the server — and the camera facade, if enabled — until
    /// `shutdown` resolves. A facade frame still in flight at shutdown
    /// is ended (`stop_capture`) before this returns.
    ///
    /// The runner ([`rusty_photon_service_lifecycle::ServiceRunner`]) owns
    /// signal installation; this method just threads the shutdown future
    /// into the serve loops (TLS when a `tls` block is configured, plain
    /// `axum::serve` otherwise).
    ///
    /// # Errors
    ///
    /// Returns an I/O error when a serve loop fails; TLS configuration
    /// and serve failures surface wrapped as I/O errors.
    pub async fn start(
        self,
        shutdown: impl Future<Output = ()> + Send + 'static,
    ) -> Result<(), std::io::Error> {
        let Self {
            listener,
            router,
            local_addr: _,
            tls,
            camera,
        } = self;
        // One shutdown, two listeners: fan it out.
        let (stop_tx, stop_rx) = tokio::sync::watch::channel(false);
        tokio::spawn(async move {
            shutdown.await;
            let _ = stop_tx.send(true);
        });
        let stopped = |mut rx: tokio::sync::watch::Receiver<bool>| async move {
            let _ = rx.wait_for(|stop| *stop).await;
        };

        let guider = serve(listener, router, tls, stopped(stop_rx.clone()));
        let Some(camera) = camera else {
            return guider.await;
        };
        let BoundCamera {
            listener: camera_listener,
            router: camera_router,
            local_addr: _,
            tls: camera_tls,
            engine,
        } = camera;
        let facade = serve(camera_listener, camera_router, camera_tls, stopped(stop_rx));
        let served = tokio::try_join!(guider, facade).map(|_| ());
        engine.shutdown().await;
        served
    }
}

async fn serve(
    listener: TcpListener,
    router: axum::Router,
    tls: Option<rusty_photon_tls::config::TlsConfig>,
    shutdown: impl Future<Output = ()> + Send + 'static,
) -> Result<(), std::io::Error> {
    match tls {
        Some(ref tls_config) => {
            rusty_photon_tls::server::serve_tls(listener, router, tls_config, shutdown)
                .await
                .map_err(std::io::Error::other)
        }
        None => {
            axum::serve(listener, router)
                .with_graceful_shutdown(shutdown)
                .await
        }
    }
}

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
mod tests {
    use super::*;

    /// A config serving both listeners on OS-picked loopback ports, with
    /// the facade enabled on `image_dir`.
    fn camera_config(image_dir: &std::path::Path, unique_id: Option<&str>) -> Config {
        let mut camera = serde_json::json!({
            "enabled": true,
            "pixel_size_x_um": 2.9,
            "pixel_size_y_um": 2.9,
            "image_dir": image_dir,
            "server": { "port": 0 }
        });
        if let Some(id) = unique_id {
            camera["unique_id"] = id.into();
        }
        serde_json::from_value(serde_json::json!({
            "server": { "port": 0, "bind_address": "127.0.0.1" },
            "camera": camera,
        }))
        .unwrap()
    }

    #[tokio::test]
    async fn an_enabled_facade_binds_its_own_loopback_listener() {
        let dir = tempfile::tempdir().unwrap();
        let bound = ServerBuilder::new()
            .with_config(camera_config(dir.path(), Some("facade-test")))
            .with_config_path(dir.path().join("phd2-guider.json"))
            .build()
            .await
            .unwrap();
        let camera = bound.camera_addr().expect("the facade is bound");
        assert!(camera.ip().is_loopback(), "{camera}");
        assert_ne!(camera.port(), bound.listen_addr().port());
    }

    #[tokio::test]
    async fn a_disabled_facade_binds_nothing_more() {
        let config: Config =
            serde_json::from_value(serde_json::json!({ "server": { "port": 0 } })).unwrap();
        let bound = ServerBuilder::new()
            .with_config(config)
            .build()
            .await
            .unwrap();
        assert!(bound.camera_addr().is_none());
    }

    #[tokio::test]
    async fn an_enabled_facade_without_a_unique_id_fails_the_build() {
        let dir = tempfile::tempdir().unwrap();
        let err = ServerBuilder::new()
            .with_config(camera_config(dir.path(), None))
            .build()
            .await
            .err()
            .expect("the build succeeded without a unique_id");
        assert!(err.to_string().contains("camera.unique_id"), "{err}");
    }

    #[tokio::test]
    async fn an_enabled_facade_on_a_missing_image_dir_fails_the_build() {
        let dir = tempfile::tempdir().unwrap();
        let err = ServerBuilder::new()
            .with_config(camera_config(
                &dir.path().join("missing"),
                Some("facade-test"),
            ))
            .build()
            .await
            .err()
            .expect("the build succeeded on a missing image_dir");
        assert!(err.to_string().contains("camera.image_dir"), "{err}");
    }

    #[tokio::test]
    async fn a_build_without_a_config_fails() {
        let err = ServerBuilder::default()
            .build()
            .await
            .err()
            .expect("the build succeeded without a config");
        assert!(err.to_string().contains("config is required"), "{err}");
    }
}
