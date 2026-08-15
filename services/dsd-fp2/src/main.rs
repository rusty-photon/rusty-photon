//! Deep Sky Dad FP2 ASCOM Alpaca driver — CLI entry point.

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

use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use clap::Parser;
use dsd_fp2::{load_effective_config, CliOverrides, ServerBuilder};
use rusty_photon_service_lifecycle::{ServiceResult, ServiceRunner};
use tracing::{debug, info, Level};

#[cfg(feature = "mock")]
use dsd_fp2::MockTransportFactory;

#[derive(Parser)]
#[command(name = "dsd-fp2")]
#[command(about = "ASCOM Alpaca CoverCalibrator driver for the Deep Sky Dad FP2")]
#[command(version)]
// A top-level `--config` alongside a subcommand would parse but be
// silently ignored (the subcommand carries its own); reject the mixed
// form outright, same as rp's CLI.
#[command(args_conflicts_with_subcommands = true)]
struct Args {
    #[command(subcommand)]
    command: Option<Command>,

    /// Path to configuration file. Defaults to the platform config
    /// directory (e.g. `~/.config/rusty-photon/dsd-fp2.json` on Linux) — the
    /// default path is created on first start if absent.
    #[arg(short, long)]
    config: Option<PathBuf>,

    /// Serial port path (overrides config file)
    #[arg(long)]
    port: Option<String>,

    /// Server port (overrides config file)
    #[arg(long)]
    server_port: Option<u16>,

    /// Log level
    #[arg(short, long, default_value = "info", value_parser = parse_log_level)]
    log_level: Level,

    /// Run as a Windows service (used by the service control manager).
    /// No-op on non-Windows targets.
    #[arg(long, hide = true)]
    service: bool,
}

/// Subcommands; running with none starts the ASCOM Alpaca driver.
#[derive(clap::Subcommand)]
enum Command {
    /// Diagnose this service's configuration without starting it
    /// (docs/services/doctor.md). Read-only; exits 1 on failing checks.
    Doctor {
        /// Path to configuration file
        #[arg(short, long)]
        config: Option<PathBuf>,

        /// Print the report as JSON instead of text
        #[arg(long)]
        json: bool,
    },
}

fn parse_log_level(s: &str) -> Result<Level, String> {
    s.parse()
        .map_err(|_| format!("Invalid log level: {s}. Use: trace, debug, info, warn, error"))
}

fn main() -> ServiceResult {
    let args = Args::parse();

    if let Some(Command::Doctor { config, json }) = args.command {
        dsd_fp2::doctor::run(config, json);
    }

    // In Windows SCM service mode logs go to the rolling file under
    // %PROGRAMDATA%\rusty-photon\logs\; hold the guard until process exit so
    // the final lines flush on SCM Stop. Console mode logs to stderr as before.
    let _tracing_guard = rusty_photon_service_lifecycle::init_service_tracing(
        "dsd-fp2",
        args.log_level,
        args.service,
    );

    debug!(
        "Parsed command line arguments: config={:?}, port={:?}, server_port={:?}, log_level={:?}",
        args.config, args.port, args.server_port, args.log_level
    );

    // Bootstrap the config file: materialize the default on first start and mint
    // the device's stable ASCOM `UniqueID` into `cover_calibrator.unique_id`,
    // before the reload loop reads the config — so the id is on disk by the time
    // the server reads it.
    let config_path = rusty_photon_config::resolve_and_init(
        "dsd-fp2",
        args.config,
        &serde_json::to_value(dsd_fp2::Config::default())?,
        &["/cover_calibrator/unique_id"],
    )?;
    let overrides = CliOverrides {
        serial_port: args.port,
        server_port: args.server_port,
    };

    info!("Starting Deep Sky Dad FP2 driver");
    #[cfg(feature = "mock")]
    info!("Running in MOCK MODE - no real hardware");
    info!("Configuration path: {}", config_path.display());

    // `config.apply` triggers an in-process reload rather than a process bounce:
    // each loop iteration re-reads the effective config and rebuilds the server.
    ServiceRunner::new("dsd-fp2")
        .with_reload()
        .scm_mode(args.service)
        .run_with_reload(move |shutdown, reload| async move {
            loop {
                let config = load_effective_config(&config_path, &overrides)?;
                debug!(
                    "Loaded effective configuration: serial.port={}, server.port={}",
                    config.serial.port, config.server.port
                );

                #[cfg(feature = "mock")]
                let bound = {
                    let factory = Arc::new(MockTransportFactory::default());
                    ServerBuilder::new()
                        .with_config(config)
                        .with_factory(factory)
                        .with_config_source(config_path.clone(), overrides.clone())
                        .with_reload_signal(reload.clone())
                        .build()
                        .await?
                };

                #[cfg(not(feature = "mock"))]
                let bound = ServerBuilder::new()
                    .with_config(config)
                    .with_config_source(config_path.clone(), overrides.clone())
                    .with_reload_signal(reload.clone())
                    .build()
                    .await?;

                // Stop the server on either the service shutdown or a config
                // reload. We await `start()` to completion (rather than dropping
                // it on reload) so its teardown runs — gracefully draining HTTP
                // connections and calling `transport.shutdown()` to release the
                // serial port and reconnect supervisor before we rebuild.
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
                    debug!("Reload signalled; rebuilding dsd-fp2 from the new configuration");
                    continue;
                }
                return Ok(());
            }
        })
}
