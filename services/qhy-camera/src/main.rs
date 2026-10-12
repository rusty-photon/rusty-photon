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

use clap::{Parser, Subcommand};
use qhy_camera::{load_effective_config, serve_and_rebuild, CliOverrides, ServerBuilder};
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
    /// exercise the zero-camera startup path (contract C0) — with a
    /// `usb_devices` list, to empty the SDK's side of the join (U7). Only
    /// meaningful when built with `--features simulation`.
    #[cfg(feature = "simulation")]
    #[arg(long, hide = true)]
    simulation_empty: bool,

    /// Test-only: the simulated camera leaves the bus whenever this file
    /// exists and comes back when it is removed, to exercise a camera that
    /// loses its power while connected (contract C9). Only meaningful when
    /// built with `--features simulation`.
    #[cfg(feature = "simulation")]
    #[arg(long, hide = true, value_name = "PATH")]
    simulation_departure_file: Option<PathBuf>,

    /// Test-only: stage the USB scan a `usb_devices` list is placed from with
    /// the document in this file (docs/services/doctor.md "USB inventory"),
    /// read again at every scan; without it a simulation build stages one
    /// record for the simulated camera (U7).
    #[cfg(feature = "simulation")]
    #[arg(long, hide = true, value_name = "PATH")]
    usb_inventory: Option<PathBuf>,

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
    Doctor(DoctorArgs),
}

#[derive(clap::Args)]
struct DoctorArgs {
    /// Path to configuration file
    #[arg(short, long)]
    config: Option<PathBuf>,

    /// Print the report as JSON instead of text
    #[arg(long)]
    json: bool,

    /// List this driver's cameras on the bus by USB port, and print the
    /// `usb_devices` block to paste, instead of the report. Read-only: no
    /// camera is opened.
    #[arg(long, conflicts_with = "json")]
    devices: bool,

    /// Test-only: the staged USB inventory, as for the service. Only
    /// `--devices` reads the bus, so it is refused without it.
    #[cfg(feature = "simulation")]
    #[arg(long, hide = true, value_name = "PATH", requires = "devices")]
    usb_inventory: Option<PathBuf>,
}

fn parse_log_level(s: &str) -> std::result::Result<Level, String> {
    s.parse()
        .map_err(|_| format!("invalid log level: {s}. Use: trace, debug, info, warn, error"))
}

fn main() -> ServiceResult {
    let args = Args::parse();

    if let Some(Command::Doctor(doctor)) = args.command {
        if doctor.devices {
            #[cfg(feature = "simulation")]
            let source = match qhy_camera::claims::usb_source_from_flag(doctor.usb_inventory) {
                Ok(source) => source,
                Err(error) => {
                    eprintln!("doctor: {error}");
                    std::process::exit(2);
                }
            };
            #[cfg(not(feature = "simulation"))]
            let source = qhy_camera::UsbSource::Host;
            qhy_camera::doctor::run_devices(doctor.config, &source);
        }
        qhy_camera::doctor::run(doctor.config, doctor.json);
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
    let config_path = rusty_photon_config::resolve_and_init::<qhy_camera::Config>(
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
    #[cfg(feature = "simulation")]
    let simulation_departure_file = args.simulation_departure_file;
    #[cfg(feature = "simulation")]
    let usb_source = qhy_camera::claims::usb_source_from_flag(args.usb_inventory)
        .map_err(qhy_camera::QhyCameraError::UsbInventory)?;

    // Startup chatter stays at debug! per docs/AGENTS.md Rule 9; the
    // user-facing "Service started successfully on <addr>" info! lives in
    // lib.rs.
    debug!("Starting QHY camera driver");
    #[cfg(feature = "simulation")]
    debug!("Using the qhyccd-rs SIMULATION backend");
    debug!("Configuration path: {}", config_path.display());

    // `config.apply` triggers an in-process reload: the loop re-reads the
    // effective config, re-enumerates, and rebuilds the server. A failed USB
    // scan's re-scan rebuilds it from the configuration it serves (U6).
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
            serve_and_rebuild(
                shutdown.token(),
                reload.clone(),
                || {
                    let config = load_effective_config(&config_path, &overrides)?;
                    debug!(
                        "Loaded effective configuration: server.port={}",
                        config.server.port
                    );
                    Ok(config)
                },
                |config, stop| {
                    let builder = ServerBuilder::new()
                        .with_config(config)
                        .with_config_source(config_path.clone(), overrides.clone())
                        .with_reload_signal(reload.clone());
                    #[cfg(feature = "simulation")]
                    let builder = {
                        let builder = builder.with_usb_source(usb_source.clone());
                        if simulation_empty {
                            builder.with_sdk(qhyccd_rs::Sdk::new_simulated())
                        } else if let Some(departure) = &simulation_departure_file {
                            let mut sdk = qhyccd_rs::Sdk::new_simulated();
                            sdk.add_simulated_camera(
                                qhyccd_rs::simulation::SimulatedCameraConfig::sdk_default()
                                    .with_departure_file(departure),
                            );
                            builder.with_sdk(sdk)
                        } else {
                            builder
                        }
                    };
                    async move { builder.build().await?.start(stop).await }
                },
            )
            .await?;
            Ok(())
        })
}
