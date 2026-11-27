#![cfg_attr(coverage_nightly, feature(coverage_attribute))]
#![cfg_attr(
    test,
    allow(
        clippy::arithmetic_side_effects,
        clippy::as_conversions,
        clippy::indexing_slicing
    )
)]
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

//! guider: rusty-photon's own guider (docs/services/guider.md). A headless
//! service that is an Alpaca client of one Camera and one Telescope and
//! serves `rp` an HTTP API. It is idle after every start: nothing but a
//! client's request makes it expose or pulse.

pub mod api;
pub mod calibration_store;
pub mod camera_loop;
pub mod config;
pub mod devices;
pub mod doctor;
pub mod error;
pub mod guide_log;
pub mod guider;
pub mod mount;
pub mod preview;
pub mod routes;
pub mod sensor;
pub mod setup;
#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
mod test_camera;
#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
mod test_mount;

use std::future::Future;
use std::net::SocketAddr;
use std::path::Path;
use std::sync::Arc;

use tokio_util::sync::CancellationToken;
use tracing::{debug, info};

use crate::calibration_store::CalibrationStore;
use crate::config::Config;
use crate::devices::{DeviceClient, DevicePoll};
use crate::error::{GuiderError, Result};
use crate::guider::Guider;

/// Builder for the guider server.
pub struct ServerBuilder {
    config: Option<Config>,
    setup: Option<setup::Pages>,
}

impl ServerBuilder {
    #[must_use]
    pub const fn new() -> Self {
        Self {
            config: None,
            setup: None,
        }
    }

    #[must_use]
    pub fn with_config(mut self, config: Config) -> Self {
        self.config = Some(config);
        self
    }

    /// Serve the setup page at `/setup`, behind the listener's TLS and
    /// Basic Auth (guider.md § Setup page).
    #[must_use]
    pub fn with_setup_pages(mut self, pages: setup::Pages) -> Self {
        self.setup = Some(pages);
        self
    }

    /// Consume the builder: check what the configuration names on disk,
    /// build the device clients, assemble the router and bind the
    /// configured listen address. Nothing here calls a device.
    ///
    /// # Errors
    ///
    /// Returns [`Config`](GuiderError::Config) if no config was supplied
    /// or the platform state directory cannot be resolved,
    /// [`CaCert`](GuiderError::CaCert) if `ca_cert` cannot be read or
    /// holds no certificate, [`StateDir`](GuiderError::StateDir) if the
    /// state directory cannot be created or written,
    /// [`DeviceClient`](GuiderError::DeviceClient) if a device client
    /// cannot be built, or [`Io`](GuiderError::Io) if the listener cannot
    /// be bound or its address read.
    pub async fn build(self) -> Result<BoundServer> {
        let config = self.config.ok_or_else(|| {
            GuiderError::Config(
                "ServerBuilder::build: config is required \u{2014} call .with_config(...) first"
                    .to_owned(),
            )
        })?;

        if let Some(path) = config.ca_cert_path() {
            check_ca_cert(path)?;
        }
        let state_dir = config.state_dir()?;
        prepare_state_dir(&state_dir)?;
        debug!(path = %state_dir.display(), "state directory ready");

        let ca_cert = config.ca_cert_path();
        let camera = DeviceClient::of(&config.devices.camera, ca_cert)?.map(Arc::new);
        let mount = DeviceClient::of(&config.devices.mount, ca_cert)?.map(Arc::new);
        let (poll, devices) = DevicePoll::new(
            camera.clone(),
            mount.clone(),
            config.devices.poll_interval.get(),
        );

        let server = config.server.clone();
        let config = Arc::new(config);
        let store = Arc::new(CalibrationStore::open(&state_dir));
        let guide_log = guide_log::GuideLog::new(&state_dir, config.guide_log);
        let guider = Arc::new(Guider::new(
            Arc::clone(&config),
            camera,
            mount,
            devices.clone(),
            store,
            guide_log,
        ));
        let router =
            routes::build_router(routes::AppState::new(config, devices, Arc::clone(&guider)));
        let router = match self.setup {
            Some(pages) => router.merge(setup::router(pages)),
            None => router,
        };
        // Layer HTTP Basic Auth when configured (server.auth): it guards
        // the API, the setup page and /health alike.
        let router = match &server.auth {
            Some(auth) => {
                if server.tls.is_none() {
                    tracing::warn!(
                        "Authentication is enabled but TLS is not. Credentials will be \
                         transmitted in cleartext. Consider enabling TLS (see `doctor --fix`)."
                    );
                }
                rp_auth::layer(router, auth)
            }
            None => router,
        };

        let listener = tokio::net::TcpListener::bind(server.socket_addr()).await?;
        let local_addr = listener.local_addr()?;

        // This println is parsed by BDD tests to discover the bound port.
        // Console mode only: stdout is a dead handle under the Windows SCM,
        // and the only stdout consumer (bdd-infra's port parser) never runs
        // services with --service.
        if !rusty_photon_service_lifecycle::is_scm_service() {
            println!("Bound guider server bound_addr={local_addr}");
        }
        info!("guider service bound on {}", local_addr);

        Ok(BoundServer {
            listener,
            router,
            local_addr,
            tls: server.tls,
            poll,
            guider,
        })
    }
}

impl Default for ServerBuilder {
    fn default() -> Self {
        Self::new()
    }
}

/// `ca_cert` holds at least one certificate.
pub(crate) fn check_ca_cert(path: &Path) -> Result<()> {
    let failed = |message: String| GuiderError::CaCert {
        path: path.to_path_buf(),
        message,
    };
    let pem = std::fs::read(path).map_err(|e| failed(format!("cannot be read: {e}")))?;
    let certificates = rustls_pemfile::certs(&mut pem.as_slice())
        .collect::<std::result::Result<Vec<_>, _>>()
        .map_err(|e| failed(format!("does not parse as PEM: {e}")))?;
    if certificates.is_empty() {
        return Err(failed("holds no certificate".to_owned()));
    }
    Ok(())
}

/// Create the state directory and prove it can be written to.
pub(crate) fn prepare_state_dir(path: &Path) -> Result<()> {
    let failed = |message: String| GuiderError::StateDir {
        path: path.to_path_buf(),
        message,
    };
    std::fs::create_dir_all(path).map_err(|e| failed(format!("cannot be created: {e}")))?;
    let probe = path.join(format!(".guider-write-check-{}", std::process::id()));
    std::fs::write(&probe, b"").map_err(|e| failed(format!("cannot be written: {e}")))?;
    std::fs::remove_file(&probe).map_err(|e| failed(format!("cannot be written: {e}")))?;
    Ok(())
}

/// A fully bound guider server ready to accept connections.
pub struct BoundServer {
    listener: tokio::net::TcpListener,
    router: axum::Router,
    local_addr: SocketAddr,
    tls: Option<rusty_photon_tls::config::TlsConfig>,
    poll: DevicePoll,
    guider: Arc<Guider>,
}

impl BoundServer {
    #[must_use]
    pub const fn listen_addr(&self) -> SocketAddr {
        self.local_addr
    }

    /// Poll the devices and serve the bound listener until `shutdown`
    /// resolves, then stop the loop: a request that waits on it answers
    /// `stopped`, and the guider ends its own exposure.
    ///
    /// # Errors
    ///
    /// Returns [`Server`](GuiderError::Server) if the TLS material cannot be
    /// loaded or the serve loop fails, and
    /// [`LoopEnded`](GuiderError::LoopEnded) if the camera loop's task
    /// ended without a stop.
    pub async fn start(self, shutdown: impl Future<Output = ()> + Send + 'static) -> Result<()> {
        info!("guider service started on {}", self.local_addr);

        let stop_poll = CancellationToken::new();
        let poll = tokio::spawn(self.poll.run(stop_poll.clone()));

        let ended_abnormally = self.guider.ended_abnormally();
        let guider = Arc::clone(&self.guider);
        let watched = ended_abnormally.clone();
        let shutdown = async move {
            tokio::select! {
                () = shutdown => {}
                () = watched.cancelled() => {}
            }
            if let Err(error) = guider.stop().await {
                debug!(error = %error.message, "the loop did not end before the shutdown");
            }
            guider.close_guide_log();
        };

        let served = match self.tls {
            Some(ref tls) => {
                rusty_photon_tls::server::serve_tls(self.listener, self.router, tls, shutdown)
                    .await
                    .map_err(|e| GuiderError::Server(e.to_string()))
            }
            None => axum::serve(self.listener, self.router)
                .with_graceful_shutdown(shutdown)
                .await
                .map_err(|e| GuiderError::Server(e.to_string())),
        };

        stop_poll.cancel();
        if let Err(error) = poll.await {
            debug!(%error, "the device poll ended abnormally");
        }
        debug!("guider service shut down");
        served?;
        if ended_abnormally.is_cancelled() {
            return Err(GuiderError::LoopEnded);
        }
        Ok(())
    }
}

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
mod tests {
    use super::*;

    #[test]
    fn a_ca_cert_that_does_not_exist_cannot_be_read() {
        let dir = tempfile::tempdir().unwrap();
        let error = check_ca_cert(&dir.path().join("ca.pem")).unwrap_err();
        assert!(error.to_string().contains("cannot be read"), "{error}");
    }

    #[test]
    fn a_ca_cert_with_no_certificate_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("ca.pem");
        std::fs::write(&path, "not a certificate").unwrap();
        let error = check_ca_cert(&path).unwrap_err();
        assert!(
            error.to_string().ends_with("holds no certificate"),
            "{error}"
        );
    }

    #[test]
    fn a_ca_cert_with_a_certificate_is_accepted() {
        let dir = tempfile::tempdir().unwrap();
        rusty_photon_tls::test_cert::generate_ca(dir.path()).unwrap();
        check_ca_cert(&rusty_photon_tls::config::ca_cert_path(dir.path())).unwrap();
    }

    #[test]
    fn the_state_directory_is_created_with_its_parents_and_left_empty() {
        let dir = tempfile::tempdir().unwrap();
        let state = dir.path().join("a").join("b");
        prepare_state_dir(&state).unwrap();
        assert!(state.is_dir());
        assert_eq!(std::fs::read_dir(&state).unwrap().count(), 0);
    }

    #[test]
    fn a_state_directory_under_a_file_cannot_be_created() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("file");
        std::fs::write(&file, b"").unwrap();
        let error = prepare_state_dir(&file.join("state")).unwrap_err();
        assert!(error.to_string().contains("cannot be created"), "{error}");
    }

    #[tokio::test]
    async fn a_build_without_a_config_is_refused() {
        let Err(error) = ServerBuilder::new().build().await else {
            panic!("a build without a config bound a server");
        };
        assert!(error.to_string().contains("config is required"), "{error}");
    }
}
