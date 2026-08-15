//! Sentinel CLI
//!
//! Command-line interface for the observatory monitoring and notification service.

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
use sentinel::{load_config, Config, SentinelBuilder};
use tracing::Level;

#[derive(Parser)]
#[command(name = "sentinel")]
#[command(about = "Observatory monitoring and notification service")]
#[command(version)]
// A top-level `--config` alongside a subcommand would parse but be
// silently ignored (the subcommand carries its own); reject the mixed
// form outright, same as rp's CLI.
#[command(args_conflicts_with_subcommands = true)]
struct Args {
    #[command(subcommand)]
    command: Option<Command>,

    /// Path to configuration file. Defaults to the platform config
    /// directory (e.g. `~/.config/rusty-photon/sentinel.json` on Linux);
    /// created with defaults on first start if absent.
    #[arg(short, long)]
    config: Option<PathBuf>,

    /// Dashboard port (overrides the config file's `server.port`)
    #[arg(long)]
    dashboard_port: Option<u16>,

    /// Log level
    #[arg(short, long, default_value = "info")]
    log_level: Level,

    /// Run as a Windows service (used by the service control manager).
    /// No-op on non-Windows targets.
    #[arg(long, hide = true)]
    service: bool,
}

/// Subcommands; running with none starts the sentinel service.
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

fn main() -> ServiceResult {
    let args = Args::parse();

    // Before tracing init: doctor writes its report to stdout and exits.
    if let Some(Command::Doctor { config, json }) = args.command {
        sentinel::doctor::run(config, json);
    }

    // In Windows SCM service mode logs go to the rolling file under
    // %PROGRAMDATA%\rusty-photon\logs\; hold the guard until process exit so
    // the final lines flush on SCM Stop. Console mode logs to stderr as before.
    let _tracing_guard = rusty_photon_service_lifecycle::init_service_tracing(
        "sentinel",
        args.log_level,
        args.service,
    );

    tracing::debug!(
        "Parsed command line arguments: config={:?}, dashboard_port={:?}, log_level={:?}",
        args.config,
        args.dashboard_port,
        args.log_level
    );

    let config_path = rusty_photon_config::resolve_and_init(
        "sentinel",
        args.config,
        &serde_json::to_value(Config::default())?,
        &[],
    )?;
    tracing::debug!("Loading configuration from {:?}", config_path);
    let mut config = load_config(&config_path)?;

    config.resolve_secrets()?;

    if let Some(dashboard_port) = args.dashboard_port {
        config.server.port = dashboard_port;
    }

    tracing::info!("Starting sentinel service");
    tracing::debug!(
        "Monitors: {}, Notifiers: {}, Transitions: {}",
        config.monitors.len(),
        config.notifiers.len(),
        config.transitions.len()
    );

    // Discovery derives probe URLs from the supervised services' own
    // `<svc>.json` files, which live next to sentinel's own config file.
    let config_dir = config_path.parent().map(std::path::Path::to_path_buf);

    ServiceRunner::new("sentinel")
        .scm_mode(args.service)
        .run(move |shutdown| async move {
            let mut builder =
                SentinelBuilder::new(config).with_cancellation_token(shutdown.token());
            if let Some(dir) = config_dir {
                builder = builder.with_config_dir(dir);
            }
            builder.build().await?.start().await?;
            Ok(())
        })
}
