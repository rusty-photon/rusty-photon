#![cfg_attr(coverage_nightly, feature(coverage_attribute))]
#![cfg_attr(
    test,
    allow(
        clippy::arithmetic_side_effects,
        clippy::as_conversions,
        clippy::indexing_slicing
    )
)]
//! # zwo-focuser — ASCOM Alpaca driver for the ZWO EAF
//!
//! The service enumerates every connected EAF via [`zwo_rs`] and registers
//! each as an ASCOM [`ZwoFocuser`] device on one port, with a serial-derived
//! `UniqueID`. Each device implements the full `Device` + `Focuser` surface
//! (absolute move, position, is-moving, halt, live temperature) over the
//! [`backend::FocuserHandle`] SDK seam — the same native-SDK architecture as
//! `zwo-camera`, not the serial `rusty-photon-shared-transport` pattern
//! `qhy-focuser`/`pa-scops-oag` use.
//!
//! See `docs/services/zwo-focuser.md` for the full design and
//! `docs/plans/zwo-driver.md` for the decision record.
//!
//! ## Native dependency
//!
//! `zwo-rs` is built with only its `focuser` feature (ADR-014), so this binary
//! links exactly `libEAFFocuser` (+ `libudev` on Linux) — machines compiling
//! this package need that SDK installed, even with the `simulation` feature,
//! which removes the *focuser*, not the *link*. The libudev link is
//! deliberate: the EAF blob references `udev_*` symbols without declaring
//! libudev in its own `DT_NEEDED`, so this binary must carry the dependency
//! for the loader to resolve them at startup — `libudev-dev`/`systemd-devel`
//! at build time, `libudev1` at runtime (see ADR-014).

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
mod config;
mod config_actions;
pub mod doctor;
mod error;
mod focuser;

pub use config::{
    load_effective_config, AlpacaServerConfig, CliOverrides, Config, DeviceOverride, DEFAULT_PORT,
};
pub use config_actions::ZwoFocuserDriver;
pub use error::ZwoFocuserError;
pub use focuser::ZwoFocuser;

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
use zwo_rs::FocuserInfo;

use crate::backend::{FocuserHandle, ZwoFocuserHandle};

/// One EAF discovered at enumeration: its index, [`FocuserInfo`], the working
/// travel limit (`EAFGetMaxStep`), the bare SDK `serial` (the key for
/// `devices` config overrides), and the serial-derived ASCOM `UniqueID`.
struct EnumeratedFocuser {
    index: usize,
    info: FocuserInfo,
    max_step: u32,
    serial: String,
    unique_id: String,
}

/// Builds a bound zwo-focuser server from an effective [`Config`].
#[derive(Default)]
pub struct ServerBuilder {
    config: Config,
    config_path: Option<PathBuf>,
    overrides: CliOverrides,
    reload: Option<ReloadSignal>,
    /// Register no focusers (the test-only zero-focuser startup path, C0).
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

    /// Register no focusers regardless of what the SDK reports — the
    /// test-only empty-backend path exercising the zero-focuser startup
    /// (contract C0).
    #[must_use]
    pub const fn with_empty(mut self, empty: bool) -> Self {
        self.force_empty = empty;
        self
    }

    /// Enumerate the connected EAFs, register each as an ASCOM device, and
    /// bind the Alpaca listener.
    ///
    /// Zero discovered EAFs is **not** a hard failure: the server starts with
    /// no Focuser devices and logs a warning (a later reload re-enumerates).
    ///
    /// # Errors
    /// Returns [`ZwoFocuserError`] when SDK enumeration fails or the listener
    /// cannot bind the configured port.
    pub async fn build(self) -> Result<BoundServer, ZwoFocuserError> {
        let focusers = if self.force_empty {
            Vec::new()
        } else {
            enumerate_focusers().await?
        };
        if focusers.is_empty() {
            warn!("no ZWO EAFs discovered; starting with no Focuser devices");
        }

        let mut server = Server::new(CargoServerInfo!());
        for eaf in &focusers {
            let handle: Arc<dyn FocuserHandle> = Arc::new(ZwoFocuserHandle::new(
                zwo_rs::Sdk::new()?,
                eaf.index,
                eaf.info.clone(),
                eaf.max_step,
                eaf.unique_id.clone(),
            ));
            // `devices` overrides are keyed by the bare SDK serial (matching the
            // config-actions `devices.{serial}` paths), NOT the prefixed
            // `ZWO:{name}:{serial}` UniqueID.
            let mut device = ZwoFocuser::new(handle, self.config.devices.get(&eaf.serial));
            if let (Some(path), Some(reload)) = (self.config_path.clone(), self.reload.clone()) {
                device = device.with_config_actions(rusty_photon_driver::ConfigActionCtx {
                    effective: self.config.clone(),
                    path,
                    overrides: self.overrides.clone(),
                    reload,
                });
            }
            debug!(device = eaf.index, name = %device.static_name(), "registering ZWO EAF");
            server.devices.register(device);
        }

        // Use the shared dual-stack helper (IPv6 + IPv4) with SO_REUSEADDR, like
        // every other Alpaca service. SO_REUSEADDR matters here because the
        // in-process `with_reload` loop rebinds the same port; a raw bind could
        // fail to rebind while a prior listener's TIME_WAIT lingers.
        let bind_addr = self.config.server.socket_addr();
        let listener = rusty_photon_tls::server::bind_dual_stack_tokio(bind_addr)
            .await
            .map_err(|source| ZwoFocuserError::Bind {
                addr: bind_addr.to_string(),
                source,
            })?;
        let local_addr = listener
            .local_addr()
            .map_err(|source| ZwoFocuserError::Bind {
                addr: bind_addr.to_string(),
                source: rusty_photon_tls::error::TlsError::Io(source),
            })?;

        // Opt-in Alpaca UDP discovery responder (config `discovery_port`);
        // bound here so a taken port fails startup, run in start().
        let discovery =
            rusty_photon_driver::discovery::bind(local_addr, self.config.server.discovery_port)
                .await
                .map_err(|e| ZwoFocuserError::Discovery(e.to_string()))?;

        let app = axum::Router::new().fallback_service(server.into_service());
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
        // discovery; every peer service emits it. Logs go to stderr, so this
        // stays parseable.
        // Console mode only: stdout is a dead handle under the Windows SCM,
        // and the only stdout consumer (bdd-infra's port parser) never runs
        // services with --service.
        if !rusty_photon_service_lifecycle::is_scm_service() {
            println!("Bound Alpaca server bound_addr={local_addr}");
        }
        info!(focusers = focusers.len(), address = %local_addr, "Service started successfully");
        Ok(BoundServer {
            listener,
            app,
            local_addr,
            tls: self.config.server.tls.clone(),
            discovery,
        })
    }
}

/// A zwo-focuser server bound to a local port, ready to [`start`](Self::start).
pub struct BoundServer {
    listener: TcpListener,
    app: axum::Router,
    local_addr: SocketAddr,
    /// TLS settings from `server.tls`; `Some` makes `start()` serve HTTPS.
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
    /// Returns [`ZwoFocuserError::Server`] if the HTTP server stops with an
    /// error.
    pub async fn start(
        self,
        shutdown: impl Future<Output = ()> + Send + 'static,
    ) -> Result<(), ZwoFocuserError> {
        let Self {
            listener,
            app,
            local_addr: _,
            tls,
            discovery,
        } = self;
        let serve = async {
            match tls {
                Some(ref tls_config) => {
                    rusty_photon_tls::server::serve_tls(listener, app, tls_config, shutdown)
                        .await
                        .map_err(|e| ZwoFocuserError::Server(e.to_string()))
                }
                None => rusty_photon_tls::server::serve_plain(listener, app, shutdown)
                    .await
                    .map_err(|e| ZwoFocuserError::Server(e.to_string())),
            }
        };
        rusty_photon_driver::discovery::serve_with(discovery, serve).await?;
        Ok(())
    }
}

/// Enumerate connected EAFs on the blocking thread pool, minting each
/// device's serial-derived `UniqueID`.
///
/// `EAFGetSerialNumber` requires an *open* focuser, so each is opened briefly
/// to read its serial and then closed (the per-device connect handshake
/// happens later on `set_connected(true)`). The EAF SDK is blocking C FFI, so
/// every SDK call funnels through [`tokio::task::spawn_blocking`] (design doc
/// "Concurrency").
async fn enumerate_focusers() -> Result<Vec<EnumeratedFocuser>, ZwoFocuserError> {
    let focusers =
        tokio::task::spawn_blocking(|| -> Result<Vec<EnumeratedFocuser>, zwo_rs::Error> {
            let sdk = zwo_rs::Sdk::new()?;
            let count = sdk.focusers()?.len();
            let mut out = Vec::with_capacity(count);
            for index in 0..count {
                // Open briefly to read the stable serial and the working
                // travel limit, then close (the focuser drops at the end of
                // the block → `EAFClose`). The post-open `FocuserInfo` is the
                // one cached: `EAFGetProperty` needs device access to fill
                // `MaxStep`, so the pre-open enumeration copy can be
                // incomplete.
                let (info, max_step, serial_result) = {
                    let focuser = sdk.open_focuser(index)?;
                    let info = focuser.info().clone();
                    // `EAFGetMaxStep` is the limit the firmware stops at;
                    // `EAF_INFO::MaxStep` is only the ceiling it can be
                    // raised to (see docs/services/zwo-focuser.md).
                    let max_step = match focuser.max_step() {
                        Ok(v) => v,
                        Err(e) => {
                            warn!(
                                focuser = %info.name, error = %e,
                                "EAFGetMaxStep failed; falling back to the EAF_INFO ceiling"
                            );
                            info.max_step
                        }
                    };
                    (info, max_step, focuser.serial())
                };
                if let Err(ref e) = serial_result {
                    warn!(
                        focuser = %info.name, error = %e,
                        "EAF exposes no hardware serial (or unsupported firmware); using a position-based identity"
                    );
                }
                let (serial, unique_id) = mint_identity(serial_result, &info.name, index);
                out.push(EnumeratedFocuser {
                    index,
                    info,
                    max_step,
                    serial,
                    unique_id,
                });
            }
            Ok(out)
        })
        .await??;
    debug!(count = focusers.len(), "enumerated ZWO EAFs");
    Ok(focusers)
}

/// Mint the `(serial, UniqueID)` pair for an enumerated focuser.
///
/// The serial is the EAF's hardware serial (`EAFGetSerialNumber`). Unlike the
/// ASI camera, the EAF SDK exposes no `ASIGetID`-equivalent second tier, so the
/// fallback on a failed serial read is directly a stable position-based
/// identity (`noserial-{index}`): unique per enumeration slot and stable
/// across reconnects for the common single-focuser case.
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
        let (serial, unique_id) = mint_identity(Ok("2a3b4c5d6e7f8091".to_owned()), "ZWO EAF", 0);
        assert_eq!(serial, "2a3b4c5d6e7f8091");
        assert_eq!(unique_id, "ZWO:ZWO-EAF:2a3b4c5d6e7f8091");
    }

    #[test]
    fn mint_identity_falls_back_to_position_when_no_serial() {
        let err = Err(zwo_rs::Error::Eaf(zwo_rs::EafError::NotSupported));
        let (serial, unique_id) = mint_identity(err, "ZWO EAF", 0);
        assert_eq!(serial, "noserial-0");
        assert_eq!(unique_id, "ZWO:ZWO-EAF:noserial-0");
    }
}

#[cfg(all(test, feature = "simulation"))]
#[cfg_attr(coverage_nightly, coverage(off))]
mod simulation_tests {
    use super::*;

    /// End-to-end proof against the `zwo-rs` simulation backend: the builder
    /// enumerates the one simulated EAF, registers it, and binds an ephemeral
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
    /// `ZWO:{name}:{serial}` `UniqueID`.
    #[tokio::test]
    async fn device_overrides_are_keyed_by_serial_not_unique_id() {
        let eaf = &enumerate_focusers().await.unwrap()[0];
        assert!(
            eaf.serial != eaf.unique_id && eaf.unique_id.ends_with(&eaf.serial),
            "UniqueID should be the prefixed serial, serial the bare key"
        );
        let mut config = Config::default();
        config.devices.insert(
            eaf.serial.clone(),
            DeviceOverride {
                name: Some("Main Focuser".to_string()),
                ..Default::default()
            },
        );
        assert!(
            config.devices.contains_key(&eaf.serial),
            "the serial must resolve the override the builder applies"
        );
        assert!(
            !config.devices.contains_key(&eaf.unique_id),
            "the UniqueID must NOT resolve it"
        );
    }

    /// The empty-backend path starts healthy with no Focuser devices (C0).
    #[tokio::test]
    async fn empty_backend_binds_with_no_focusers() {
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
