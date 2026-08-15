//! planetarium-bridge CLI — the virtual ASCOM Alpaca Telescope that turns
//! planetarium Align gestures into paused rusty-photon targets
//! (docs/services/planetarium-bridge.md).

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

use clap::Parser;
use rusty_photon_service_lifecycle::{ServiceResult, ServiceRunner};
use tracing::{debug, info, Level};

use planetarium_bridge::config::{load_config, Config};
use planetarium_bridge::ServerBuilder;

#[derive(Parser)]
#[command(name = "planetarium-bridge")]
#[command(
    about = "Virtual ASCOM Alpaca Telescope: planetarium Align gestures become paused rusty-photon targets"
)]
#[command(version)]
// A top-level `--config` alongside a subcommand would parse but be
// silently ignored (the subcommand carries its own); reject the mixed
// form outright, same as rp's CLI.
#[command(args_conflicts_with_subcommands = true)]
struct Args {
    #[command(subcommand)]
    command: Option<Command>,

    /// Path to the JSON config file. When omitted, resolves to the platform
    /// config path (e.g. `~/.config/rusty-photon/planetarium-bridge.json` on
    /// Linux).
    #[arg(short, long)]
    config: Option<PathBuf>,

    /// Log level: trace, debug, info, warn, error.
    #[arg(short, long, default_value = "info", value_parser = parse_log_level)]
    log_level: Level,

    /// Run as a Windows service (used by the service control manager).
    /// No-op on non-Windows targets.
    #[arg(long, hide = true)]
    service: bool,
}

/// Subcommands; running with none starts the virtual Telescope.
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
        .map_err(|_| format!("invalid log level: {s} (use trace, debug, info, warn, error)"))
}

fn main() -> ServiceResult {
    let args = Args::parse();

    if let Some(Command::Doctor { config, json }) = args.command {
        planetarium_bridge::doctor::run(config, json);
    }

    // In Windows SCM service mode logs go to the rolling file under
    // %PROGRAMDATA%\rusty-photon\logs\; hold the guard until process exit so
    // the final lines flush on SCM Stop. Console mode logs to stderr.
    let _tracing_guard = rusty_photon_service_lifecycle::init_service_tracing(
        "planetarium-bridge",
        args.log_level,
        args.service,
    );

    // Resolve the config path (explicit --config, else the platform config
    // dir), materialize the default config on first start, and mint the
    // device's persisted, spec-compliant `UniqueID`. Minting is idempotent
    // and never overwrites an existing id.
    let config_path = rusty_photon_config::resolve_and_init(
        "planetarium-bridge",
        args.config,
        &serde_json::to_value(Config::default())?,
        &["/device/unique_id"],
    )?;
    debug!("Resolved configuration path: {:?}", config_path);

    info!("Starting planetarium-bridge");

    ServiceRunner::new("planetarium-bridge")
        .scm_mode(args.service)
        .run(move |shutdown| async move {
            let config = load_config(&config_path)?;
            let bound = ServerBuilder::new(config).build().await?;
            bound.start(shutdown.cancelled()).await?;
            Ok(())
        })
}
