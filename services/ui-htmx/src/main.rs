//! `ui-htmx` — CLI entry point for the web configuration UI (BFF).

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

use clap::Parser;
use rusty_photon_service_lifecycle::{report_from_boxed, ServiceResult, ServiceRunner};
use tracing::{debug, info, warn, Level};
use ui_htmx::{build_router, load_config, AppState, Config};

#[derive(Parser)]
#[command(name = "ui-htmx")]
#[command(about = "Server-rendered web configuration UI (BFF) for rusty-photon")]
#[command(version)]
// A top-level `--config` alongside a subcommand would parse but be
// silently ignored (the subcommand carries its own); reject the mixed
// form outright, same as rp's CLI.
#[command(args_conflicts_with_subcommands = true)]
struct Args {
    #[command(subcommand)]
    command: Option<Command>,

    /// Path to the BFF configuration file. Defaults to the platform
    /// config directory (e.g. `~/.config/rusty-photon/ui-htmx.json` on
    /// Linux); created with defaults on first start if absent (binds
    /// 0.0.0.0:11120, targets dsd-fp2 at <http://127.0.0.1:11119>).
    #[arg(short, long)]
    config: Option<PathBuf>,

    /// BFF listen port (overrides server.port).
    #[arg(long)]
    port: Option<u16>,

    /// Log level
    #[arg(short, long, default_value = "info", value_parser = parse_log_level)]
    log_level: Level,

    /// Run as a Windows service (used by the service control manager).
    /// No-op on non-Windows targets.
    #[arg(long, hide = true)]
    service: bool,
}

/// Subcommands; running with none starts the BFF.
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

    // Before tracing init: doctor writes its report to stdout and exits.
    if let Some(Command::Doctor { config, json }) = args.command {
        ui_htmx::doctor::run(config, json);
    }

    // In Windows SCM service mode logs go to the rolling file under
    // %PROGRAMDATA%\rusty-photon\logs\; hold the guard until process exit so
    // the final lines flush on SCM Stop. Console mode logs to stderr as before.
    let _tracing_guard = rusty_photon_service_lifecycle::init_service_tracing(
        "ui-htmx",
        args.log_level,
        args.service,
    );

    let default_config = serde_json::to_value(Config::default())?;
    let config_path =
        rusty_photon_config::resolve_and_init("ui-htmx", args.config, &default_config, &[])?;
    debug!("Loading configuration from {config_path:?}");
    let mut config = load_config(&config_path).map_err(report_from_boxed)?;
    if let Some(port) = args.port {
        config.server.port = port;
    }

    info!("Starting ui-htmx configuration UI");

    ServiceRunner::new("ui-htmx")
        .scm_mode(args.service)
        .run(move |shutdown| async move {
            // Open SSE proxy streams do not end on axum's graceful-shutdown signal
            // alone (axum #2673): give the state a cancellation token and fire it
            // the moment shutdown starts, so `/stream/events` streams close and
            // axum's connection drain can complete promptly.
            let sse_token = tokio_util::sync::CancellationToken::new();
            let state = AppState::from_config(&config)?.with_sse_shutdown(sse_token.clone());
            let app = build_router(state);

            // Layer HTTP Basic auth around the whole router (`/health`
            // included) when `server.auth` is configured.
            let app = match &config.server.auth {
                Some(auth) => {
                    if config.server.tls.is_none() {
                        warn!(
                            "Authentication is enabled but TLS is not. \
                             Credentials will be transmitted in cleartext. \
                             Consider enabling TLS (see `doctor --fix`)."
                        );
                    }
                    rp_auth::layer(app, auth)
                }
                None => app,
            };

            let listener = tokio::net::TcpListener::bind(config.server.socket_addr()).await?;
            let bound = listener.local_addr()?;
            // Print to stdout (not just `info!`) so port discovery works regardless
            // of log level/output — matching the drivers' `bound_addr=` line.
            // Console mode only: stdout is a dead handle under the Windows SCM,
            // and the only stdout consumer (bdd-infra's port parser) never runs
            // services with --service.
            if !rusty_photon_service_lifecycle::is_scm_service() {
                println!("Bound ui-htmx server bound_addr={bound}");
            }
            info!("ui-htmx listening; bound_addr={bound}");

            let shutdown_signal = async move {
                shutdown.cancelled().await;
                sse_token.cancel();
            };
            match &config.server.tls {
                Some(tls) => {
                    debug!("Serving HTTPS (server.tls configured)");
                    rusty_photon_tls::server::serve_tls(listener, app, tls, shutdown_signal)
                        .await?;
                }
                None => {
                    axum::serve(listener, app)
                        .with_graceful_shutdown(shutdown_signal)
                        .await?;
                }
            }
            Ok(())
        })
}
