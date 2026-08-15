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

use clap::{Parser, Subcommand};
use rusty_photon_service_lifecycle::{
    init_service_tracing, ServiceResult, ServiceRunner, Shutdown,
};
use tracing::{debug, Level};

#[derive(Parser)]
#[command(name = "rp", about = "Rusty Photon - equipment gateway and event bus")]
// With `Serve.config` optional, `rp --config <path> serve` would otherwise
// parse the path into the top-level shorthand and silently ignore it
// (serving from the XDG default instead). Reject the mixed form outright:
// use `rp --config <path>` or `rp serve --config <path>`.
#[command(args_conflicts_with_subcommands = true)]
struct Cli {
    #[command(subcommand)]
    command: Option<Commands>,

    /// Path to configuration file (shorthand for `rp serve --config`;
    /// cannot be combined with a subcommand)
    #[arg(long)]
    config: Option<PathBuf>,

    /// Log level (trace, debug, info, warn, error)
    #[arg(long, default_value = "info", value_parser = clap::value_parser!(Level))]
    log_level: Level,

    /// Run as a Windows service (used by the service control manager).
    /// No-op on non-Windows targets.
    #[arg(long, hide = true)]
    service: bool,
}

#[derive(Subcommand)]
enum Commands {
    /// Start the rp server
    Serve {
        /// Path to configuration file. Defaults to the platform
        /// config directory (e.g. `~/.config/rusty-photon/rp.json` on
        /// Linux); created with a minimal scaffold on first start if absent.
        #[arg(long)]
        config: Option<PathBuf>,

        /// Log level (trace, debug, info, warn, error)
        #[arg(long, default_value = "info", value_parser = clap::value_parser!(Level))]
        log_level: Level,

        /// Run as a Windows service (used by the service control manager).
        /// No-op on non-Windows targets.
        #[arg(long, hide = true)]
        service: bool,
    },
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

    match cli.command {
        Some(Commands::Serve {
            config,
            log_level,
            service,
        }) => {
            // In Windows SCM service mode logs go to the rolling file under
            // %PROGRAMDATA%\rusty-photon\logs\; hold the guard until process
            // exit so the final lines flush on SCM Stop. Console mode logs to
            // stderr as before.
            let _tracing_guard = init_service_tracing("rp", log_level, service);
            run_serve(config, service)
        }
        // No tracing init: doctor writes its report to stdout and exits.
        Some(Commands::Doctor { config, json }) => rp::doctor::run(config, json),
        None => {
            // No subcommand serves (packaged units run a bare
            // `/usr/bin/rusty-photon-rp`; the Windows MSI's ServiceInstall
            // passes just `--service`); `rp --config <path>` still works
            // as a shorthand for `rp serve --config <path>`.
            let _tracing_guard = init_service_tracing("rp", cli.log_level, cli.service);
            run_serve(cli.config, cli.service)
        }
    }
}

fn run_serve(config: Option<PathBuf>, service_mode: bool) -> ServiceResult {
    // Self-creation applies only to the XDG default path — an explicit
    // `--config` naming a missing file stays a hard error.
    let config_path =
        rusty_photon_config::resolve_and_init("rp", config, &rp::config::default_scaffold(), &[])?;
    ServiceRunner::new("rp")
        .scm_mode(service_mode)
        .run(move |shutdown: Shutdown| async move {
            debug!(config_path = %config_path.display(), "loading configuration");
            let config = rp::config::load_config(&config_path)?;

            rp::ServerBuilder::new()
                .with_config(config)
                .with_config_path(config_path)
                .build()
                .await?
                .start(shutdown.cancelled())
                .await?;

            Ok(())
        })
}
