//! Pegasus Astro Scops OAG Driver CLI
//!
//! Command-line interface for the Pegasus Scops OAG ASCOM Alpaca driver.

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

use pa_scops_oag::config::{load_effective_config, CliOverrides};
#[cfg(feature = "mock")]
use pa_scops_oag::{Config, MockScopsTransportFactory, ServerBuilder};
#[cfg(not(feature = "mock"))]
use pa_scops_oag::{Config, ServerBuilder};

#[derive(Parser)]
#[command(name = "pa-scops-oag")]
#[command(about = "ASCOM Alpaca driver for the Pegasus Astro Scops OAG focuser")]
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

fn parse_log_level(s: &str) -> Result<Level, String> {
    s.parse()
        .map_err(|_| format!("Invalid log level: {s}. Use: trace, debug, info, warn, error"))
}

fn main() -> ServiceResult {
    let args = Args::parse();

    if let Some(Command::Doctor { config, json }) = args.command {
        pa_scops_oag::doctor::run(config, json);
    }

    // In Windows SCM service mode logs go to the rolling file under
    // %PROGRAMDATA%\rusty-photon\logs\; hold the guard until process exit so
    // the final lines flush on SCM Stop. Console mode logs to stderr as before.
    let _tracing_guard = rusty_photon_service_lifecycle::init_service_tracing(
        "pa-scops-oag",
        args.log_level,
        args.service,
    );

    debug!(
        "Parsed command line arguments: config={:?}, port={:?}, server_port={:?}, log_level={:?}",
        args.config, args.port, args.server_port, args.log_level
    );

    // CLI overrides are tracked (not just applied) so config.apply can keep them
    // out of the persisted file — a transient `--port` is never baked in.
    let overrides = CliOverrides {
        serial_port: args.port.clone(),
        server_port: args.server_port,
    };

    // Resolve the config path (explicit --config, else the platform
    // config dir), materialize the default config on first start, and mint the
    // focuser's persisted, spec-compliant `UniqueID` — idempotent, never
    // overwrites an existing id.
    let config_path = rusty_photon_config::resolve_and_init(
        "pa-scops-oag",
        args.config,
        &serde_json::to_value(Config::default())?,
        &["/focuser/unique_id"],
    )?;
    debug!("Resolved configuration path: {:?}", config_path);

    info!("Starting Pegasus Scops OAG driver");
    #[cfg(feature = "mock")]
    info!("Running in MOCK MODE - no real hardware");

    // Reload loop: a `config.apply` that changes a field fires the reload signal,
    // which breaks `start()` out via the combined stop future; the loop then
    // re-reads the freshly-persisted config and rebuilds the server.
    ServiceRunner::new("pa-scops-oag")
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
                    let factory = Arc::new(MockScopsTransportFactory::default());
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
                    debug!("reloading pa-scops-oag configuration");
                    continue;
                }
                return Ok(());
            }
        })
}
