//! zwo-focuser ASCOM Alpaca driver — CLI entry point.

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
use rusty_photon_service_lifecycle::{ServiceResult, ServiceRunner};
use tracing::{debug, Level};
use zwo_focuser::{load_effective_config, CliOverrides, ServerBuilder};

#[derive(Parser)]
#[command(name = "zwo-focuser")]
#[command(about = "ASCOM Alpaca driver for the ZWO EAF")]
#[command(version)]
// A top-level `--config` alongside a subcommand would parse but be
// silently ignored (the subcommand carries its own); reject the mixed
// form outright, same as rp's CLI.
#[command(args_conflicts_with_subcommands = true)]
struct Args {
    #[command(subcommand)]
    command: Option<Command>,

    /// Path to the JSON config file. When omitted, resolves to the
    /// platform config path (e.g. `~/.config/rusty-photon/zwo-focuser.json` on
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

    /// Test-only: start with an *empty* simulation backend (no focusers), to
    /// exercise the zero-focuser startup path (contract C0). Only meaningful
    /// when built with `--features simulation`.
    #[cfg(feature = "simulation")]
    #[arg(long, hide = true)]
    simulation_empty: bool,
}

/// Subcommands; running with none starts the ASCOM Alpaca driver.
#[derive(clap::Subcommand)]
enum Command {
    /// Diagnose this service's configuration and what the EAF SDK can see,
    /// without starting it (docs/services/doctor.md). Read-only; exits 1
    /// on failing checks.
    Doctor {
        /// Path to the JSON config file
        #[arg(short, long)]
        config: Option<PathBuf>,

        /// Print the report as JSON instead of text
        #[arg(long)]
        json: bool,
    },
}

fn parse_log_level(s: &str) -> Result<Level, String> {
    s.parse()
        .map_err(|_| format!("invalid log level: {s} (use trace, debug, info, warn, error)"))
}

fn main() -> ServiceResult {
    let args = Args::parse();

    if let Some(Command::Doctor { config, json }) = args.command {
        zwo_focuser::doctor::run(config, json);
    }

    // In Windows SCM service mode logs go to the rolling file under
    // %PROGRAMDATA%\rusty-photon\logs\; hold the guard until process exit so
    // the final lines flush on SCM Stop. Console mode logs to stderr as before.
    let _tracing_guard = rusty_photon_service_lifecycle::init_service_tracing(
        "zwo-focuser",
        args.log_level,
        args.service,
    );

    // Bootstrap the config file: a path is always resolvable (explicit
    // --config or the XDG default), and the default config materializes at the
    // default path on first start. The empty identity-pointer list is
    // deliberate: the ASCOM UniqueID is derived from the EAF SDK serial at
    // enumeration, not minted into config (see the design doc "Device
    // identity").
    let config_path = rusty_photon_config::resolve_and_init(
        "zwo-focuser",
        args.config,
        &serde_json::to_value(zwo_focuser::Config::default())?,
        &[],
    )?;
    let overrides = CliOverrides { port: args.port };
    #[cfg(feature = "simulation")]
    let simulation_empty = args.simulation_empty;
    debug!(config = ?config_path, "starting zwo-focuser");

    // `config.apply` triggers an in-process reload: each loop iteration re-reads
    // the effective config, re-enumerates, and rebuilds the server.
    ServiceRunner::new("zwo-focuser")
        .with_reload()
        .scm_mode(args.service)
        .run_with_reload(move |shutdown, reload| async move {
            loop {
                let config = load_effective_config(&config_path, &overrides)?;

                let builder = ServerBuilder::new()
                    .with_config(config)
                    .with_config_source(config_path.clone(), overrides.clone())
                    .with_reload_signal(reload.clone());

                #[cfg(feature = "simulation")]
                let builder = builder.with_empty(simulation_empty);

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
                    debug!("reload signalled; rebuilding zwo-focuser from the new configuration");
                    continue;
                }
                return Ok(());
            }
        })
}
