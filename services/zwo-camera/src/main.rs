//! zwo-camera ASCOM Alpaca driver — CLI entry point.

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

use clap::Parser;
use rusty_photon_service_lifecycle::{ServiceResult, ServiceRunner};
use tracing::{debug, Level};
use zwo_camera::{load_effective_config, serve_and_rebuild, CliOverrides, ServerBuilder};

#[derive(Parser)]
#[command(name = "zwo-camera")]
#[command(about = "ASCOM Alpaca driver for ZWO ASI cameras")]
#[command(version)]
// A top-level `--config` alongside a subcommand would parse but be
// silently ignored (the subcommand carries its own); reject the mixed
// form outright, same as rp's CLI.
#[command(args_conflicts_with_subcommands = true)]
struct Args {
    #[command(subcommand)]
    command: Option<Command>,

    /// Path to the JSON config file. When omitted, resolves to the
    /// platform config path (e.g. `~/.config/rusty-photon/zwo-camera.json` on
    /// Linux) via `rusty_photon_config::resolve_config_path`.
    #[arg(short, long)]
    config: Option<PathBuf>,

    /// Server port (overrides the config file).
    #[arg(long)]
    port: Option<u16>,

    /// Log level: trace, debug, info, warn, error.
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

    /// Test-only: the simulated camera leaves the bus whenever this file
    /// exists and comes back when it is removed, to exercise a camera that
    /// loses its power or cable while connected (contract C6). Only
    /// meaningful when built with `--features simulation`.
    #[cfg(feature = "simulation")]
    #[arg(long, hide = true, value_name = "PATH")]
    simulation_departure_file: Option<PathBuf>,

    /// Test-only: every open of the simulated camera fails, to exercise a
    /// camera that cannot be opened for its identity (U5). Only meaningful
    /// when built with `--features simulation`.
    #[cfg(feature = "simulation")]
    #[arg(long, hide = true)]
    simulation_unopenable: bool,

    /// Test-only: stage the USB scan a `usb_devices` list is placed from with
    /// the document in this file (docs/services/doctor.md "USB inventory"),
    /// read again at every scan; without it a simulation build stages one
    /// record per simulated camera (U7).
    #[cfg(feature = "simulation")]
    #[arg(long, hide = true, value_name = "PATH")]
    usb_inventory: Option<PathBuf>,
}

/// Subcommands; running with none starts the ASCOM Alpaca driver.
#[derive(clap::Subcommand)]
enum Command {
    /// Diagnose this service's configuration and what the ASI SDK can see,
    /// without starting it (docs/services/doctor.md). Read-only; exits 1
    /// on failing checks.
    Doctor(DoctorArgs),
}

#[derive(clap::Args)]
struct DoctorArgs {
    /// Path to the JSON config file
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

    /// Test-only: the staged USB inventory, as for the service — read by the
    /// report's `usb-devices.*` checks and by `--devices` alike.
    #[cfg(feature = "simulation")]
    #[arg(long, hide = true, value_name = "PATH")]
    usb_inventory: Option<PathBuf>,
}

fn parse_log_level(s: &str) -> Result<Level, String> {
    s.parse()
        .map_err(|_| format!("invalid log level: {s} (use trace, debug, info, warn, error)"))
}

fn main() -> ServiceResult {
    let args = Args::parse();

    if let Some(Command::Doctor(doctor)) = args.command {
        #[cfg(feature = "simulation")]
        let source = match zwo_camera::claims::usb_source_from_flag(doctor.usb_inventory) {
            Ok(source) => source,
            Err(error) => {
                eprintln!("doctor: {error}");
                std::process::exit(2);
            }
        };
        #[cfg(not(feature = "simulation"))]
        let source = zwo_camera::UsbSource::Host;
        if doctor.devices {
            zwo_camera::doctor::run_devices(doctor.config, &source);
        }
        zwo_camera::doctor::run(doctor.config, doctor.json, &source);
    }

    // In Windows SCM service mode logs go to the rolling file under
    // %PROGRAMDATA%\rusty-photon\logs\; hold the guard until process exit so
    // the final lines flush on SCM Stop. Console mode logs to stderr as before.
    let _tracing_guard = rusty_photon_service_lifecycle::init_service_tracing(
        "zwo-camera",
        args.log_level,
        args.service,
    );

    // Bootstrap the config file: a path is always resolvable (explicit
    // --config or the XDG default), and the default config materializes at the
    // default path on first start. The empty identity-pointer list is
    // deliberate: ASCOM UniqueIDs are derived from the camera SDK serials at
    // enumeration, not minted into config (see the design doc "Device
    // identity").
    let config_path = rusty_photon_config::resolve_and_init::<zwo_camera::Config>(
        "zwo-camera",
        args.config,
        &serde_json::to_value(zwo_camera::Config::default())?,
        &[],
    )?;
    let overrides = CliOverrides { port: args.port };
    #[cfg(feature = "simulation")]
    let simulation_empty = args.simulation_empty;
    #[cfg(feature = "simulation")]
    let simulation_departure_file = args.simulation_departure_file;
    #[cfg(feature = "simulation")]
    let simulation_unopenable = args.simulation_unopenable;
    #[cfg(feature = "simulation")]
    let usb_source = zwo_camera::claims::usb_source_from_flag(args.usb_inventory)
        .map_err(zwo_camera::ZwoCameraError::UsbInventory)?;
    debug!(config = ?config_path, "starting zwo-camera");

    // `config.apply` triggers an in-process reload: the loop re-reads the
    // effective config, re-enumerates, and rebuilds the server. A failed USB
    // scan's re-scan rebuilds it from the configuration it serves (U6).
    ServiceRunner::new("zwo-camera")
        .with_reload()
        .scm_mode(args.service)
        .run_with_reload(move |shutdown, reload| async move {
            serve_and_rebuild(
                shutdown.token(),
                reload.clone(),
                || load_effective_config(&config_path, &overrides),
                |config, stop| {
                    let builder = ServerBuilder::new()
                        .with_config(config)
                        .with_config_source(config_path.clone(), overrides.clone())
                        .with_reload_signal(reload.clone());
                    #[cfg(feature = "simulation")]
                    let builder = builder
                        .with_empty(simulation_empty)
                        .with_departure_file(simulation_departure_file.clone())
                        .with_unopenable(simulation_unopenable)
                        .with_usb_source(usb_source.clone());
                    async move { builder.build().await?.start(stop).await }
                },
            )
            .await?;
            Ok(())
        })
}
