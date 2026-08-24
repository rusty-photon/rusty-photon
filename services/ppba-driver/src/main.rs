//! PPBA Switch Driver CLI
//!
//! Command-line interface for the Pegasus Astro Pocket Powerbox Advance Gen2 Switch driver.

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
use rust_embed::RustEmbed;
use rusty_photon_i18n::{fl, fluent_language_loader, LocalizedParser};
use rusty_photon_service_lifecycle::{ServiceResult, ServiceRunner};
use tracing::Level;

use ppba_driver::config::{load_effective_config, CliOverrides};
#[cfg(feature = "mock")]
use ppba_driver::{Config, MockPpbaTransportFactory, ServerBuilder};
#[cfg(not(feature = "mock"))]
use ppba_driver::{Config, ServerBuilder};

#[derive(RustEmbed)]
#[folder = "i18n/"]
struct Localizations;

#[derive(Parser, LocalizedParser)]
#[command(name = "ppba-driver")]
#[command(version)]
// A top-level `--config` alongside a subcommand would parse but be
// silently ignored (the subcommand carries its own); reject the mixed
// form outright, same as rp's CLI.
#[command(args_conflicts_with_subcommands = true)]
#[localized(about = "cli-about")]
struct Args {
    #[command(subcommand)]
    command: Option<Command>,

    /// Path to configuration file
    #[arg(short, long)]
    #[localized(help = "cli-help-config")]
    config: Option<PathBuf>,

    /// Serial port path (overrides config file)
    #[arg(long)]
    #[localized(help = "cli-help-port")]
    port: Option<String>,

    /// Server port (overrides config file)
    #[arg(long)]
    #[localized(help = "cli-help-server-port")]
    server_port: Option<u16>,

    /// Enable/disable Switch device
    #[arg(long)]
    #[localized(help = "cli-help-enable-switch")]
    enable_switch: Option<bool>,

    /// Enable/disable `ObservingConditions` device
    #[arg(long)]
    #[localized(help = "cli-help-enable-observingconditions")]
    enable_observingconditions: Option<bool>,

    /// Log level
    #[arg(short, long, default_value = "info", value_parser = parse_log_level)]
    #[localized(help = "cli-help-log-level")]
    log_level: Level,

    /// Run as a Windows service (used by the service control manager).
    /// No-op on non-Windows targets. Hidden, so deliberately not localized.
    #[arg(long, hide = true)]
    service: bool,
}

/// Subcommands. Help text is not localized — the `LocalizedParser` derive
/// localizes only the top-level args (rp's CLI sets the same precedent).
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
    s.parse().map_err(|_| {
        rusty_photon_i18n::fl_active(|loader| fl!(loader, "error-invalid-log-level", value = s))
            .unwrap_or_else(|| {
                format!("Invalid log level: {s}. Use: trace, debug, info, warn, error")
            })
    })
}

fn main() -> ServiceResult {
    let (loader, i18n_status) = rusty_photon_i18n::init(fluent_language_loader!(), &Localizations);
    let args = Args::parse_localized(&loader);

    if let Some(Command::Doctor { config, json }) = args.command {
        ppba_driver::doctor::run(config, json);
    }

    // Setup tracing. In Windows SCM service mode logs go to the rolling file
    // under %PROGRAMDATA%\rusty-photon\logs\; hold the guard until process exit
    // so the final lines flush on SCM Stop. Console mode logs to stderr as before.
    let _tracing_guard = rusty_photon_service_lifecycle::init_service_tracing(
        "ppba-driver",
        args.log_level,
        args.service,
    );

    match i18n_status {
        Ok(()) => {}
        Err(rusty_photon_i18n::LoadError::Available { reason }) => {
            tracing::warn!(
                %reason,
                "i18n: failed to enumerate embedded locales; running with English fallback"
            );
        }
        Err(rusty_photon_i18n::LoadError::Load { reason }) => {
            tracing::warn!(
                %reason,
                "i18n: failed to load negotiated locale bundle; running with English fallback"
            );
        }
        Err(rusty_photon_i18n::LoadError::AlreadyInitialized) => {
            // Distinct from the load-failure cases: the loader is *not*
            // English-fallback-only, it's just whatever the first init
            // populated. Surfaces the most likely cause (refactor or test
            // artefact) so it's visible without misrepresenting the locale.
            tracing::warn!(
                "i18n: rusty_photon_i18n::init was called more than once on this thread; \
                 second call's loader was discarded, active locale unchanged"
            );
        }
    }

    tracing::debug!(
        "Parsed command line arguments: config={:?}, port={:?}, server_port={:?}, log_level={:?}",
        args.config,
        args.port,
        args.server_port,
        args.log_level
    );

    // Bootstrap the config file: materialize the default on first start and mint
    // a UUIDv4 `UniqueID` for each device, so the subsequent load always
    // succeeds. Minting is idempotent — it only fills empty/absent ids and
    // never overwrites an existing one.
    let config_path = rusty_photon_config::resolve_and_init(
        "ppba-driver",
        args.config,
        &serde_json::to_value(Config::default())?,
        &["/switch/unique_id", "/observingconditions/unique_id"],
    )?;
    tracing::debug!("Resolved configuration path: {:?}", config_path);

    // CLI overrides are tracked (not just applied) so config.apply keeps them
    // out of the persisted file — a transient `--port` / `--enable-switch` is
    // never baked in.
    let overrides = CliOverrides {
        serial_port: args.port.clone(),
        server_port: args.server_port,
        enable_switch: args.enable_switch,
        enable_observingconditions: args.enable_observingconditions,
    };

    tracing::info!("Starting PPBA driver");
    #[cfg(feature = "mock")]
    tracing::info!("Running in MOCK MODE - no real hardware");

    // Reload loop: a `config.apply` that changes a field fires the reload signal
    // (from either device); the loop re-reads the freshly-persisted config and
    // rebuilds the server, awaiting `start()` to completion so the old server
    // drains HTTP and releases the serial port before the rebuilt one binds.
    ServiceRunner::new("ppba-driver")
        .with_reload()
        .scm_mode(args.service)
        .run_with_reload(move |shutdown, reload| async move {
            loop {
                let config = load_effective_config(&config_path, &overrides)?;
                #[cfg(not(feature = "mock"))]
                tracing::info!("Serial port: {}", config.serial.port);
                tracing::info!("Server port: {}", config.server.port);

                #[cfg(feature = "mock")]
                let bound = {
                    let factory = Arc::new(MockPpbaTransportFactory::default());
                    ServerBuilder::new(config)
                        .with_factory(factory)
                        .with_config_source(config_path.clone(), overrides.clone())
                        .with_reload_signal(reload.clone())
                        .build()
                        .await?
                };
                #[cfg(not(feature = "mock"))]
                let bound = ServerBuilder::new(config)
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
                    tracing::debug!("reloading ppba-driver configuration");
                    continue;
                }
                return Ok(());
            }
        })
}
