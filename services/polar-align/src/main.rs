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
use tracing::{debug, Level};

#[derive(Parser)]
#[command(
    name = "polar-align",
    about = "Plate-solving polar alignment orchestrator - three-point measurement with live adjustment"
)]
// A top-level `--config` alongside a subcommand would parse but be
// silently ignored (the subcommand carries its own); reject the mixed
// form outright, same as rp's CLI.
#[command(args_conflicts_with_subcommands = true)]
struct Cli {
    #[command(subcommand)]
    command: Option<Command>,

    /// Path to the configuration file. Defaults to the platform
    /// config directory (e.g. `~/.config/rusty-photon/polar-align.json`
    /// on Linux). There is no built-in default config: the file must
    /// exist (site coordinates are operator-provided).
    #[arg(long)]
    config: Option<PathBuf>,

    /// Port to listen on (overrides the config file's `server.port`,
    /// default 11172)
    #[arg(long)]
    port: Option<u16>,

    /// Bind address (overrides the config file's `server.bind_address`,
    /// default `0.0.0.0`)
    #[arg(long)]
    bind_address: Option<std::net::IpAddr>,

    /// Log level (trace, debug, info, warn, error)
    #[arg(long, default_value = "info", value_parser = clap::value_parser!(Level))]
    log_level: Level,

    /// Run as a Windows service (used by the service control manager).
    /// No-op on non-Windows targets.
    #[arg(long, hide = true)]
    service: bool,
}

/// Subcommands; running with none starts the HTTP service.
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
    let cli = Cli::parse();

    if let Some(Command::Doctor { config, json }) = cli.command {
        polar_align::doctor::run(config, json);
    }

    // In Windows SCM service mode logs go to the rolling file under
    // %PROGRAMDATA%\rusty-photon\logs\; hold the guard until process exit so
    // the final lines flush on SCM Stop. Console mode logs to stderr as before.
    let _tracing_guard = rusty_photon_service_lifecycle::init_service_tracing(
        "polar-align",
        cli.log_level,
        cli.service,
    );

    let config_path = rusty_photon_config::resolve_config_path("polar-align", cli.config)?;
    let overrides = polar_align::config::CliOverrides {
        port: cli.port,
        bind_address: cli.bind_address,
    };

    ServiceRunner::new("polar-align")
        .scm_mode(cli.service)
        .run(move |shutdown| async move {
            debug!(config_path = %config_path.display(), "loading configuration");
            let mut config = polar_align::config::load_config(&config_path)?;
            overrides.apply(&mut config);

            polar_align::ServerBuilder::new()
                .with_config(config)
                .build()
                .await?
                .start(shutdown.cancelled())
                .await?;

            Ok(())
        })
}
