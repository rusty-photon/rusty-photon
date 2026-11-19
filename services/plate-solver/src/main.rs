//! plate-solver service binary.
//!
//! Reads a JSON config file (`--config`, or when omitted the
//! platform config path — `~/.config/rusty-photon/plate-solver.json` on
//! Linux), validates it, builds the HTTP server, prints
//! `bound_addr=<host>:<port>` to stdout (so `bdd-infra::ServiceHandle`
//! can discover the bound port), and serves until SIGTERM / Ctrl-C.

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

use clap::Parser;
use plate_solver::{load_config, ServerBuilder};
use rusty_photon_service_lifecycle::{init_service_tracing, ServiceRunner};
use std::path::PathBuf;
use std::process::ExitCode;
use tracing::Level;

#[derive(Parser, Debug)]
#[command(
    name = "plate-solver",
    version,
    about = "rp-managed plate solver service"
)]
// A top-level `--config` alongside a subcommand would parse but be
// silently ignored (the subcommand carries its own); reject the mixed
// form outright, same as rp's CLI.
#[command(args_conflicts_with_subcommands = true)]
struct Cli {
    #[command(subcommand)]
    command: Option<Command>,

    /// Path to the JSON config file. Defaults to the platform
    /// config directory (e.g. `~/.config/rusty-photon/plate-solver.json`
    /// on Linux). There is no built-in default config: the file must
    /// exist (`astap_binary_path` / `astap_db_directory` are mandatory).
    #[arg(short, long)]
    config: Option<PathBuf>,

    /// Log level (trace, debug, info, warn, error)
    #[arg(long, default_value = "info", value_parser = clap::value_parser!(Level))]
    log_level: Level,

    /// Run as a Windows service (used by the service control manager).
    /// No-op on non-Windows targets.
    #[arg(long, hide = true)]
    service: bool,
}

/// Subcommands; running with none starts the HTTP service.
#[derive(clap::Subcommand, Debug)]
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

fn main() -> ExitCode {
    let cli = Cli::parse();

    if let Some(Command::Doctor { config, json }) = cli.command {
        plate_solver::doctor::run(config, json);
    }

    // In Windows SCM service mode logs go to the rolling file under
    // %PROGRAMDATA%\rusty-photon\logs\; hold the guard until process exit so
    // the final lines flush on SCM Stop. Console mode logs to stderr.
    let _tracing_guard = init_service_tracing("plate-solver", cli.log_level, cli.service);

    // A startup config error (exit 2). `resolve_config_path` fails only
    // when no home directory is resolvable at all (HOME unset/empty and
    // no passwd entry for the uid — a stripped-container state tests
    // can't reproduce).
    let config_path = match rusty_photon_config::resolve_config_path("plate-solver", cli.config) {
        Ok(path) => path,
        Err(e) => {
            // Through tracing, not eprintln!: in console mode the subscriber
            // writes to stderr anyway, and in Windows SCM service mode stderr
            // is a dead handle — tracing is what lands in the rolling log file.
            tracing::error!("plate-solver: {e}");
            return ExitCode::from(2);
        }
    };

    let result = ServiceRunner::new("plate-solver")
        .scm_mode(cli.service)
        // No default config to write: under the SCM the service stays
        // stopped until the operator writes one.
        .condition_path_exists(&config_path)
        .run(move |shutdown| async move {
            let config = load_config(&config_path).map_err(
                |e| -> Box<dyn std::error::Error + Send + Sync> {
                    Box::from(format!("config: {e}"))
                },
            )?;
            let server = ServerBuilder::new()
                .with_config(config)
                .build()
                .await
                .map_err(|e| -> Box<dyn std::error::Error + Send + Sync> {
                    Box::from(format!("build: {e}"))
                })?;

            let addr = server.listen_addr();
            // `bound_addr=` is parsed by `bdd-infra::follow_bound_port` to
            // discover the test-spawned service's port. Must be on stdout.
            // Console mode only: stdout is a dead handle under the Windows SCM,
            // and the only stdout consumer (bdd-infra's port parser) never runs
            // services with --service.
            if !rusty_photon_service_lifecycle::is_scm_service() {
                println!("bound_addr={addr}");
            }
            tracing::info!(%addr, "plate-solver listening");

            server.start(shutdown.cancelled()).await.map_err(
                |e| -> Box<dyn std::error::Error + Send + Sync> {
                    Box::from(format!("server: {e}"))
                },
            )?;
            Ok(())
        });

    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            let msg = e.to_string();
            // `{e:?}` on the runner's `Report` prints the full multi-line
            // error chain (ADR-011), matching what the other services get
            // by returning `ServiceResult` from `main`. Through tracing, not
            // eprintln!: console mode reaches stderr via the subscriber, and
            // in SCM service mode this lands in the rolling log file instead
            // of a dead handle.
            tracing::error!("plate-solver: {e:?}");
            // Config and build failures exit 2, runtime errors 1. The
            // closure surfaces all of them via the runner's `Report`; the
            // "config: " and "build: " prefixes it tags the first two with
            // tell them apart.
            if msg.starts_with("config: ") || msg.starts_with("build: ") {
                ExitCode::from(2)
            } else {
                ExitCode::from(1)
            }
        }
    }
}
