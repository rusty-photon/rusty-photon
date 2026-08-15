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

use clap::Parser;
use rusty_photon_service_lifecycle::{ServiceResult, ServiceRunner};
use sky_survey_camera::run_reloadable;
use std::path::PathBuf;
use tracing::{debug, Level};

#[derive(Parser)]
#[command(name = "sky-survey-camera")]
#[command(about = "ASCOM Alpaca Camera simulator backed by NASA SkyView")]
// A top-level `--config` alongside a subcommand would parse but be
// silently ignored (the subcommand carries its own); reject the mixed
// form outright, same as rp's CLI.
#[command(args_conflicts_with_subcommands = true)]
struct Args {
    #[command(subcommand)]
    command: Option<Command>,

    /// Path to the JSON config file. When omitted, resolves to the
    /// platform config path (`~/.config/rusty-photon/sky-survey-camera.json`
    /// on Linux) via `rusty_photon_config::resolve_config_path`. Pass
    /// `--config config.json` to keep the previous CWD-relative default.
    #[arg(short, long)]
    config: Option<PathBuf>,

    #[arg(short, long, default_value = "info", value_parser = clap::value_parser!(Level))]
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
        /// Path to the JSON config file
        #[arg(short, long)]
        config: Option<PathBuf>,

        /// Print the report as JSON instead of text
        #[arg(long)]
        json: bool,
    },
}

fn main() -> ServiceResult {
    let args = Args::parse();

    if let Some(Command::Doctor { config, json }) = args.command {
        sky_survey_camera::doctor::run(config, json);
    }

    // In Windows SCM service mode logs go to the rolling file under
    // %PROGRAMDATA%\rusty-photon\logs\; hold the guard until process exit so
    // the final lines flush on SCM Stop. Console mode logs to stderr as before.
    let _tracing_guard = rusty_photon_service_lifecycle::init_service_tracing(
        "sky-survey-camera",
        args.log_level,
        args.service,
    );

    let config_path = rusty_photon_config::resolve_config_path("sky-survey-camera", args.config)?;
    debug!(config = ?config_path, "starting sky-survey-camera");

    ServiceRunner::new("sky-survey-camera")
        .with_reload()
        .scm_mode(args.service)
        .run_with_reload(move |shutdown, reload| async move {
            run_reloadable(&config_path, shutdown, reload)
                .await
                .map_err(|e| -> Box<dyn std::error::Error + Send + Sync> { Box::new(e) })
        })
}
