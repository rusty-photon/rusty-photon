#![cfg_attr(coverage_nightly, feature(coverage_attribute))]
#![cfg_attr(
    test,
    allow(
        clippy::arithmetic_side_effects,
        clippy::as_conversions,
        clippy::indexing_slicing
    )
)]
//! sky-survey-camera: ASCOM Alpaca Camera simulator backed by NASA `SkyView`.

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

// Internal-only: holds the shared `build_alpaca_client` helper
// (`pub(crate)`), used by `mount` and `rotator`. Not part of the public
// API, so the module is private.
mod alpaca;
pub mod camera;
pub mod config;
pub mod config_actions;
pub mod doctor;
pub mod error;
pub mod fits;
#[cfg(feature = "mock")]
pub mod mock;
pub mod mount;
pub mod pointing;
pub mod rotator;
pub mod routes;
pub mod survey;

pub use config::{load_config, Config};
pub use error::SkySurveyCameraError;
#[cfg(feature = "mock")]
pub use mock::MockSurveyClient;
pub use survey::SurveyClient;

use ascom_alpaca::api::CargoServerInfo;
use ascom_alpaca::Server;
use rusty_photon_service_lifecycle::{ReloadSignal, Shutdown};
use std::future::Future;
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use crate::config_actions::SkySurveyCameraDriver;
use rusty_photon_driver::ConfigActionCtx;

/// Bind the ASCOM Alpaca Camera server (with the `/sky-survey/*`
/// custom routes composed in front of it), print `bound_addr=` for
/// the BDD harness, and serve until `shutdown` resolves.
///
/// # Errors
///
/// Returns [`SkySurveyCameraError::Identity`] for a failed identity
/// materialization; [`load_config`]'s failures;
/// [`SkySurveyCameraError::MountClient`] /
/// [`SkySurveyCameraError::RotatorClient`] for a follow-mode Alpaca
/// client that cannot be built; [`SkySurveyCameraError::Bind`] for the
/// listener or the opt-in discovery responder; and
/// [`SkySurveyCameraError::Server`] for a survey HTTP client that
/// cannot be built or a serve-loop failure.
pub async fn run(
    config_path: &Path,
    shutdown: impl Future<Output = ()> + Send + 'static,
) -> Result<(), SkySurveyCameraError> {
    // Mint a spec-compliant ASCOM UniqueID on first run and persist it
    // at `/device/unique_id`. `materialize_identity` is idempotent —
    // it only fills an absent/empty id and never overwrites an existing
    // one. Only run it when the file is present: a missing config is a
    // hard error in `load_config` because the `optics` fields are
    // mandatory and there is no `Config::default()` to scaffold from,
    // so writing an identity-only file here would just defer the same
    // failure. The call is brief blocking I/O at startup; doing it
    // directly before the load `await` is acceptable.
    if config_path.exists() {
        let outcome = rusty_photon_config::materialize_identity(
            config_path,
            &serde_json::Value::Object(serde_json::Map::new()),
            &["/device/unique_id"],
        )?;
        tracing::debug!(
            path = ?config_path,
            wrote = outcome.wrote,
            filled = ?outcome.filled,
            "materialized device identity"
        );
    }
    let config = load_config(config_path).await?;
    let survey_client = build_survey_client(&config)?;
    run_with_client(config, survey_client, shutdown).await
}

/// Reload-aware entry point used by `main`: materialize the identity
/// once, then serve in a config-reload loop.
///
/// Each cycle loads the config, serves until shutdown OR a
/// `config.apply`-fired reload breaks out, and on reload rebuilds from
/// the freshly-persisted file. Awaiting the serve to completion lets
/// the old server's graceful shutdown drain before the rebuilt one
/// binds.
///
/// # Errors
///
/// Returns the same classes as [`run`], from the initial identity
/// materialization or from any cycle's load / build / bind / serve — a
/// failure on any reload cycle ends the loop.
pub async fn run_reloadable(
    config_path: &Path,
    shutdown: Shutdown,
    reload: ReloadSignal,
) -> Result<(), SkySurveyCameraError> {
    // Mint a spec-compliant ASCOM UniqueID on first run (idempotent). Only when
    // the file exists — a missing config is a hard error in `load_config`.
    if config_path.exists() {
        let outcome = rusty_photon_config::materialize_identity(
            config_path,
            &serde_json::Value::Object(serde_json::Map::new()),
            &["/device/unique_id"],
        )?;
        tracing::debug!(path = ?config_path, wrote = outcome.wrote, filled = ?outcome.filled, "materialized device identity");
    }

    loop {
        let config = load_config(config_path).await?;
        let survey_client = build_survey_client(&config)?;
        let ctx: ConfigActionCtx<SkySurveyCameraDriver> = ConfigActionCtx {
            effective: config.clone(),
            path: config_path.to_path_buf(),
            overrides: (),
            reload: reload.clone(),
        };

        // Stop on shutdown OR a config.apply-fired reload, recording which fired.
        let reloaded = Arc::new(AtomicBool::new(false));
        let stop = {
            let reloaded = Arc::clone(&reloaded);
            let shutdown = shutdown.cancelled();
            let reload = reload.clone();
            async move {
                tokio::select! {
                    () = shutdown => {}
                    () = reload.recv() => reloaded.store(true, Ordering::SeqCst),
                }
            }
        };
        run_with_client_ctx(config, survey_client, Some(ctx), stop).await?;

        if reloaded.load(Ordering::SeqCst) {
            tracing::debug!("reloading sky-survey-camera configuration");
            continue;
        }
        return Ok(());
    }
}

/// Variant of [`run`] used by tests / the `mock` feature where the
/// caller has already constructed an [`Arc<dyn SurveyClient>`].
///
/// # Errors
///
/// Returns [`run`]'s device build, bind, and serve failures (the
/// config / identity / survey-client steps are the caller's here).
pub async fn run_with_client(
    config: Config,
    survey_client: Arc<dyn SurveyClient>,
    shutdown: impl Future<Output = ()> + Send + 'static,
) -> Result<(), SkySurveyCameraError> {
    run_with_client_ctx(config, survey_client, None, shutdown).await
}

/// As [`run_with_client`], but attaches an optional config-action context to the
/// registered camera device so it advertises `config.get`/`apply`/`schema`.
///
/// # Errors
///
/// Returns [`run_with_client`]'s device build, bind, and serve
/// failures.
pub async fn run_with_client_ctx(
    config: Config,
    survey_client: Arc<dyn SurveyClient>,
    config_ctx: Option<ConfigActionCtx<SkySurveyCameraDriver>>,
    shutdown: impl Future<Output = ()> + Send + 'static,
) -> Result<(), SkySurveyCameraError> {
    let device = build_device(config.clone(), survey_client)?;
    let device = match config_ctx {
        Some(ctx) => device.with_config_actions(ctx),
        None => device,
    };
    let shared_state = device.shared_state();

    let mut server = Server::new(CargoServerInfo!());
    server.listen_addr = config.server.socket_addr();
    server.devices.register(device);

    let alpaca_service = server.into_service();
    let app = routes::build_router(shared_state).fallback_service(alpaca_service);
    let app = match &config.server.auth {
        Some(auth) => {
            if config.server.tls.is_none() {
                tracing::warn!(
                    "Authentication is enabled but TLS is not. \
                     Credentials will be transmitted in cleartext. \
                     Consider enabling TLS (see `doctor --fix`)."
                );
            }
            rp_auth::layer(app, auth)
        }
        None => app,
    };

    let bind_addr = config.server.socket_addr();
    let listener = rusty_photon_tls::server::bind_dual_stack_tokio(bind_addr)
        .await
        .map_err(|e| SkySurveyCameraError::Bind(format!("bind {bind_addr}: {e}")))?;
    let local = listener
        .local_addr()
        .map_err(|e| SkySurveyCameraError::Bind(format!("local_addr: {e}")))?;

    // Opt-in Alpaca UDP discovery responder (config `discovery_port`);
    // bound here so a taken port fails startup, run alongside serve below.
    let discovery = rusty_photon_driver::discovery::bind(local, config.server.discovery_port)
        .await
        .map_err(|e| SkySurveyCameraError::Bind(format!("alpaca discovery responder: {e}")))?;
    // Console mode only: stdout is a dead handle under the Windows SCM,
    // and the only stdout consumer (bdd-infra's port parser) never runs
    // services with --service.
    if !rusty_photon_service_lifecycle::is_scm_service() {
        println!("bound_addr={local}");
    }
    tracing::info!(address = %local, "sky-survey-camera serving");

    // Graceful shutdown on Ctrl+C / SIGTERM. Required so coverage
    // profraw files flush when bdd-infra's ServiceHandle sends
    // SIGTERM at the end of each scenario.
    let serve = async {
        match config.server.tls {
            Some(ref tls_config) => {
                rusty_photon_tls::server::serve_tls(listener, app, tls_config, shutdown)
                    .await
                    .map_err(|e| SkySurveyCameraError::Server(e.to_string()))
            }
            None => rusty_photon_tls::server::serve_plain(listener, app, shutdown)
                .await
                .map_err(|e| SkySurveyCameraError::Server(e.to_string())),
        }
    };
    rusty_photon_driver::discovery::serve_with(discovery, serve).await?;
    Ok(())
}

/// Construct the production [`SurveyClient`] from config. The `mock`
/// feature does NOT short-circuit this — production binaries always
/// hit the configured endpoint via [`survey::SkyViewClient`]. The
/// `mock` module exposes [`MockSurveyClient`] and the
/// [`mock::synthetic_fits`] helper as library-only types: the
/// `ConformU` integration test re-uses `synthetic_fits` inside an
/// in-process axum stub that the binary fetches from over HTTP, and
/// `MockSurveyClient` is available to any future test that prefers
/// to call [`run_with_client`] directly with a synthetic backend.
/// Switching the binary itself would break the BDD scenarios that
/// exercise real HTTP error paths against a stub server.
fn build_survey_client(config: &Config) -> Result<Arc<dyn SurveyClient>, SkySurveyCameraError> {
    let client = survey::SkyViewClient::new(&config.survey)
        .map_err(|e| SkySurveyCameraError::Server(e.to_string()))?;
    Ok(Arc::new(client))
}

/// Construct a [`camera::SkySurveyCamera`] from config, selecting
/// `PointingSource::{Static, Telescope}` based on whether
/// `pointing.telescope` is set. F3 — telescope-following construction
/// must succeed even if the mount is unreachable; the Alpaca client
/// build is offline and the actual mount discovery happens lazily on
/// the first exposure.
fn build_device(
    config: Config,
    survey_client: Arc<dyn SurveyClient>,
) -> Result<camera::SkySurveyCamera, SkySurveyCameraError> {
    use crate::pointing::{
        PointingSource, PointingState, RotatorReader, SharedPointing, TelescopeFollow,
    };

    let last_snapshot = Arc::new(SharedPointing::new(PointingState::new(
        config.pointing.initial_ra_deg,
        config.pointing.initial_dec_deg,
        config.pointing.initial_rotation_deg,
    )));

    let pointing_source = match &config.pointing.telescope {
        None => PointingSource::Static(Arc::clone(&last_snapshot)),
        Some(t) => {
            let reader = mount::AlpacaMountReader::from_config(t)?;
            // F8: when `pointing.rotator` is set, source `rotation_deg`
            // from the rotator instead of the static initial value.
            // Config validation guarantees the rotator only appears in
            // follow mode, so this is the only place it's wired. Like
            // the mount client, construction is offline (F3) — a wedged
            // rotator surfaces lazily on the first exposure.
            let rotator: Option<Arc<dyn RotatorReader>> = match &config.pointing.rotator {
                None => None,
                Some(r) => {
                    let rr = rotator::AlpacaRotatorReader::from_config(r)?;
                    tracing::debug!(
                        alpaca_url = %r.alpaca_url,
                        device_number = r.device_number,
                        "rotator follow source armed"
                    );
                    Some(Arc::new(rr))
                }
            };
            let follow = TelescopeFollow::new(
                Arc::new(reader),
                rotator,
                config.pointing.initial_rotation_deg,
                t.offset_ra_arcsec,
                t.offset_dec_arcsec,
            );
            tracing::debug!(
                alpaca_url = %t.alpaca_url,
                device_number = t.device_number,
                offset_ra_arcsec = t.offset_ra_arcsec,
                offset_dec_arcsec = t.offset_dec_arcsec,
                "telescope follow mode armed"
            );
            PointingSource::Telescope(follow)
        }
    };

    Ok(camera::SkySurveyCamera::from_parts(
        config,
        survey_client,
        pointing_source,
        last_snapshot,
    ))
}
