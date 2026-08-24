#![cfg_attr(coverage_nightly, feature(coverage_attribute))]
//! Pegasus Falcon Rotator Driver CLI

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

use clap::Parser;
use rusty_photon_service_lifecycle::{ServiceResult, ServiceRunner};
use tracing::{debug, info, Level};

use pa_falcon_rotator::config::{load_effective_config, CliOverrides};
#[cfg(feature = "mock")]
use pa_falcon_rotator::{Config, MockFalconTransportFactory, ServerBuilder};
#[cfg(not(feature = "mock"))]
use pa_falcon_rotator::{Config, ServerBuilder};

#[derive(Parser)]
#[command(name = "pa-falcon-rotator")]
#[command(about = "ASCOM Alpaca driver for Pegasus Astro Falcon Rotator (firmware >= 1.3)")]
#[command(version)]
// A top-level `--config` alongside a subcommand would parse but be
// silently ignored (the subcommand carries its own); reject the mixed
// form outright, same as rp's CLI.
#[command(args_conflicts_with_subcommands = true)]
struct Args {
    #[command(subcommand)]
    command: Option<Command>,

    /// Path to configuration file
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

#[cfg_attr(coverage_nightly, coverage(off))]
fn parse_log_level(s: &str) -> Result<Level, String> {
    s.parse()
        .map_err(|_| format!("Invalid log level: {s}. Use: trace, debug, info, warn, error"))
}

#[cfg_attr(coverage_nightly, coverage(off))]
fn main() -> ServiceResult {
    let args = Args::parse();

    if let Some(Command::Doctor { config, json }) = args.command {
        pa_falcon_rotator::doctor::run(config, json);
    }

    // In Windows SCM service mode logs go to the rolling file under
    // %PROGRAMDATA%\rusty-photon\logs\; hold the guard until process exit so
    // the final lines flush on SCM Stop. Console mode logs to stderr as before.
    let _tracing_guard = rusty_photon_service_lifecycle::init_service_tracing(
        "pa-falcon-rotator",
        args.log_level,
        args.service,
    );

    debug!(
        "Parsed command line arguments: config={:?}, port={:?}, server_port={:?}, log_level={:?}",
        args.config, args.port, args.server_port, args.log_level
    );

    // CLI overrides are tracked (not just applied) so config.apply keeps them
    // out of the persisted file — a transient `--port` is never baked in.
    let overrides = CliOverrides {
        serial_port: args.port.clone(),
        server_port: args.server_port,
    };

    // Resolve the config path (explicit --config, else the platform
    // config dir) and ensure both devices have a persisted, spec-compliant
    // `UniqueID`s — materializing the default config on first start. Minting is
    // idempotent, never overwrites an existing id, and operates on the on-disk
    // file only, so a transient `--port`/`--server-port` override is never
    // baked in.
    let config_path = rusty_photon_config::resolve_and_init(
        "pa-falcon-rotator",
        args.config,
        &serde_json::to_value(Config::default())?,
        &["/rotator/unique_id", "/switch/unique_id"],
    )?;
    debug!("Resolved configuration path: {:?}", config_path);

    info!("Starting pa-falcon-rotator driver");
    #[cfg(feature = "mock")]
    info!("Running in MOCK MODE - no real hardware");

    // Reload loop: a `config.apply` that changes a field fires the reload signal
    // (from either device), which breaks `start()` out via the combined stop
    // future; the loop re-reads the freshly-persisted config and rebuilds the
    // server. Awaiting `start()` to completion lets the old server drain HTTP and
    // release the serial port before the rebuilt one binds.
    ServiceRunner::new("pa-falcon-rotator")
        .with_reload()
        .scm_mode(args.service)
        .run_with_reload(move |shutdown, reload| async move {
            loop {
                let config = load_effective_config(&config_path, &overrides)?;
                #[cfg(not(feature = "mock"))]
                info!("Serial port: {}", config.serial.port);
                info!("Server port: {}", config.server.port);

                #[cfg(feature = "mock")]
                let bound = {
                    let factory = Arc::new(MockFalconTransportFactory::default());
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
                    debug!("reloading pa-falcon-rotator configuration");
                    continue;
                }
                return Ok(());
            }
        })
}
