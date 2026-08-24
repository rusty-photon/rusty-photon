//! qhy-camera ASCOM Alpaca driver — CLI entry point.

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

use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use clap::{Parser, Subcommand};
use qhy_camera::{load_effective_config, CliOverrides, ServerBuilder};
use rusty_photon_service_lifecycle::{ServiceResult, ServiceRunner};
use tracing::{debug, Level};

#[derive(Parser)]
#[command(name = "qhy-camera")]
#[command(about = "ASCOM Alpaca Camera (+ FilterWheel) driver for QHYCCD hardware")]
#[command(version)]
// A top-level `--config` alongside a subcommand would parse but be
// silently ignored (the subcommand carries its own); reject the mixed
// form outright, same as rp's CLI.
#[command(args_conflicts_with_subcommands = true)]
struct Args {
    /// Path to configuration file. Defaults to the platform config
    /// directory (e.g. `~/.config/rusty-photon/qhy-camera.json` on Linux).
    #[arg(short, long)]
    config: Option<PathBuf>,

    /// Server port (overrides `server.port` in the config file).
    #[arg(long)]
    port: Option<u16>,

    /// Log level (trace, debug, info, warn, error).
    #[arg(short, long, default_value = "info", value_parser = parse_log_level)]
    log_level: Level,

    /// Run as a Windows service (used by the service control manager).
    /// No-op on non-Windows targets.
    #[arg(long, hide = true)]
    service: bool,

    /// Test-only: start with an *empty* simulation backend (no cameras), to
    /// exercise the zero-camera startup path (contract C0). Only meaningful when
    /// built with `--features simulation`.
    #[cfg(feature = "simulation")]
    #[arg(long, hide = true)]
    simulation_empty: bool,

    /// Subcommand; running with none starts the ASCOM Alpaca driver.
    #[command(subcommand)]
    command: Option<Command>,
}

#[derive(Subcommand)]
enum Command {
    /// Diagnose this service's configuration and what the QHYCCD SDK can
    /// see, without starting it (docs/services/doctor.md). Read-only; exits
    /// 1 on failing checks. On Windows this also diagnoses the QHYCCD
    /// installation: qhyccd.dll resolution and the loaded SDK version vs.
    /// the pinned build-time version (see docs/services/qhy-camera.md
    /// § "Windows: qhyccd.dll resolution").
    Doctor {
        /// Path to configuration file
        #[arg(short, long)]
        config: Option<PathBuf>,

        /// Print the report as JSON instead of text
        #[arg(long)]
        json: bool,
    },
}

fn parse_log_level(s: &str) -> std::result::Result<Level, String> {
    s.parse()
        .map_err(|_| format!("invalid log level: {s}. Use: trace, debug, info, warn, error"))
}

fn main() -> ServiceResult {
    let args = Args::parse();

    if let Some(Command::Doctor { config, json }) = args.command {
        qhy_camera::doctor::run(config, json);
    }

    // In Windows SCM service mode logs go to the rolling file under
    // %PROGRAMDATA%\rusty-photon\logs\; hold the guard until process exit so
    // the final lines flush on SCM Stop. Console mode logs to stderr as before.
    let _tracing_guard = rusty_photon_service_lifecycle::init_service_tracing(
        "qhy-camera",
        args.log_level,
        args.service,
    );

    // Bootstrap the config file (the default config materializes at the
    // default path on first start). The empty identity-pointer list is
    // deliberate: ASCOM UniqueIDs are derived from the camera/CFW SDK serials
    // at enumeration (see docs/services/qhy-camera.md), not minted.
    let config_path = rusty_photon_config::resolve_and_init(
        "qhy-camera",
        args.config,
        &serde_json::to_value(qhy_camera::Config::default())?,
        &[],
    )?;
    let overrides = CliOverrides {
        server_port: args.port,
    };
    #[cfg(feature = "simulation")]
    let simulation_empty = args.simulation_empty;

    // Startup chatter stays at debug! per docs/AGENTS.md Rule 9; the
    // user-facing "Service started successfully on <addr>" info! lives in
    // lib.rs.
    debug!("Starting QHY camera driver");
    #[cfg(feature = "simulation")]
    debug!("Using the qhyccd-rs SIMULATION backend");
    debug!("Configuration path: {}", config_path.display());

    ServiceRunner::new("qhy-camera")
        .with_reload()
        .scm_mode(args.service)
        .run_with_reload(move |shutdown, reload| async move {
            // Windows real-SDK builds delay-load qhyccd.dll (see build.rs):
            // resolve it BEFORE any SDK call and keep it resident, or fail
            // with the one distinctive, actionable error (PF1–PF4). This
            // runs INSIDE the run closure, not in main before the runner: in
            // SCM mode the wrapper registers and reports Running before the
            // closure, so a missing DLL becomes a clean ServiceSpecific(1)
            // stop + 5 s restart (the missing-serial-device contract). An
            // exit before SCM registration is a start FAILURE — msiexec
            // aborts the whole install with error 1920 during StartServices.
            // Simulation builds skip this: their real FFI is cfg'd out, so
            // nothing ever calls into qhyccd.dll (PF5).
            #[cfg(all(windows, not(feature = "simulation")))]
            qhy_camera::preflight::ensure_qhyccd_dll()?;
            loop {
                let config = load_effective_config(&config_path, &overrides)?;
                debug!(
                    "Loaded effective configuration: server.port={}",
                    config.server.port
                );

                let builder = ServerBuilder::new()
                    .with_config(config)
                    .with_config_source(config_path.clone(), overrides.clone())
                    .with_reload_signal(reload.clone());

                #[cfg(feature = "simulation")]
                let builder = if simulation_empty {
                    builder.with_sdk(qhyccd_rs::Sdk::new_simulated())
                } else {
                    builder
                };

                let bound = builder.build().await?;

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
                bound.start(stop).await?;

                if reloaded.load(Ordering::SeqCst) {
                    debug!("Reload signalled; rebuilding qhy-camera from the new configuration");
                    continue;
                }
                return Ok(());
            }
        })
}
