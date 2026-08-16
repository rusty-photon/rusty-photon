#![cfg_attr(coverage_nightly, feature(coverage_attribute))]
//! Star Adventurer `GTi` driver CLI.

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
use tracing::{debug, info, Level};

#[cfg(feature = "mock")]
use star_adventurer_gti::transport::mock::CapturingMockFactory;
use star_adventurer_gti::{
    canonicalise_config_path, load_config, warn_if_park_path_unwritable, Config, ServerBuilder,
    TransportFactory,
};

#[derive(Parser)]
#[command(name = "star-adventurer-gti")]
#[command(about = "ASCOM Alpaca driver for Sky-Watcher Star Adventurer GTi GEM")]
#[command(version)]
// A top-level `--config` alongside a subcommand would parse but be
// silently ignored (the subcommand carries its own); reject the mixed
// form outright, same as rp's CLI.
#[command(args_conflicts_with_subcommands = true)]
struct Args {
    #[command(subcommand)]
    command: Option<Command>,

    /// Path to JSON configuration file
    #[arg(short, long)]
    config: Option<PathBuf>,

    /// Override `transport.kind` (`usb` or `udp`)
    #[arg(long)]
    transport: Option<String>,

    /// Override `transport.port` (USB device path) or `transport.address` (UDP host)
    #[arg(long)]
    port: Option<String>,

    /// Override `transport.baud_rate` (USB only)
    #[arg(long)]
    baud: Option<u32>,

    /// Override `server.port`
    #[arg(long)]
    server_port: Option<u16>,

    /// Log level (trace / debug / info / warn / error)
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
        star_adventurer_gti::doctor::run(config, json);
    }

    // In Windows SCM service mode logs go to the rolling file under
    // %PROGRAMDATA%\rusty-photon\logs\; hold the guard until process exit so
    // the final lines flush on SCM Stop. Console mode logs to stderr as before.
    let _tracing_guard = rusty_photon_service_lifecycle::init_service_tracing(
        "star-adventurer-gti",
        args.log_level,
        args.service,
    );

    debug!(
        "Parsed command line arguments: config={:?}, transport={:?}, port={:?}, server_port={:?}, log_level={:?}",
        args.config, args.transport, args.port, args.server_port, args.log_level
    );

    // Bootstrap a real config-file path up front: the explicit `--config`
    // path if given, else the platform config dir
    // (`~/.config/rusty-photon/star-adventurer-gti.json` on Linux). A path
    // is *always* resolvable, so identity persistence and park persistence
    // are never disabled merely for lack of a `--config` flag. A fresh
    // install gets a complete, valid config file — the serialized
    // `Config::default()` with a spec-compliant UUIDv4 minted into
    // `mount.unique_id` — on its very first launch; an existing id is
    // never overwritten.
    let config_path = rusty_photon_config::resolve_and_init(
        "star-adventurer-gti",
        args.config.clone(),
        &serde_json::to_value(Config::default())?,
        &["/mount/unique_id"],
    )?;
    debug!("Resolved config path: {:?}", config_path);

    // Park persistence + config.apply target the *same* resolved config file.
    // Canonicalise it once so writes hit a stable absolute location even if the
    // process later `chdir`s, and run the early-warning writability probe.
    let config_file_path = canonicalise_config_path(&config_path);
    warn_if_park_path_unwritable(&config_file_path);

    info!("Starting Star Adventurer GTi driver");
    #[cfg(feature = "mock")]
    info!("Running in MOCK MODE - no real hardware");

    // Reload loop: a `config.apply` that changes a field fires the reload signal;
    // the loop re-reads + re-applies the CLI overrides and rebuilds the server,
    // awaiting `start()` to completion so the old server drains before rebind.
    ServiceRunner::new("star-adventurer-gti")
        .with_reload()
        .scm_mode(args.service)
        .run_with_reload(move |shutdown, reload| async move {
            loop {
                // The file always exists (materialize wrote the scaffold on
                // first run). Re-read + re-apply overrides each cycle.
                let mut config = load_config(&config_path)?;
                apply_cli_overrides(&mut config, &args)?;
                info!("Server port: {}", config.server.port);
                // Deliberately `info!` (not `debug!`): a negative floor
                // permits below-horizon pointing, and support
                // transcripts should show that relaxed state.
                let floor = config.mount.min_altitude_degrees.value();
                if floor < 0.0 {
                    info!(
                        "min_altitude_degrees is negative ({floor}°): below-horizon \
                         slew/sync targets are permitted"
                    );
                }

                let builder = ServerBuilder::new()
                    .with_config(config)
                    .with_config_file_path(Some(config_file_path.clone()))
                    .with_reload_signal(reload.clone());

                #[cfg(feature = "mock")]
                let builder = {
                    // CapturingMockFactory shares its `MockMountState` Arc with
                    // every transport; reuse it for /debug/v1/mock-commands.
                    let factory = CapturingMockFactory::new();
                    let state = Arc::clone(&factory.state);
                    let factory: Arc<dyn TransportFactory> = Arc::new(factory);
                    builder
                        .with_transport_factory(factory)
                        .with_debug_mock_state(state)
                };
                #[cfg(not(feature = "mock"))]
                {
                    // Production picks Serial vs UDP from `config.transport`.
                    let _: Option<Arc<dyn TransportFactory>> = None;
                }

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
                    debug!("reloading star-adventurer-gti configuration");
                    continue;
                }
                return Ok(());
            }
        })
}

fn apply_cli_overrides(
    config: &mut Config,
    args: &Args,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    use star_adventurer_gti::{TransportConfig, UdpConfig, UsbConfig};

    if let Some(kind) = &args.transport {
        match kind.as_str() {
            "usb" => {
                if !matches!(config.transport, TransportConfig::Usb(_)) {
                    config.transport = TransportConfig::Usb(UsbConfig::default());
                }
            }
            "udp" => {
                if !matches!(config.transport, TransportConfig::Udp(_)) {
                    config.transport = TransportConfig::Udp(UdpConfig::default());
                }
            }
            other => return Err(format!("invalid --transport: {other}").into()),
        }
    }

    if let Some(port) = &args.port {
        match &mut config.transport {
            TransportConfig::Usb(usb) => usb.port.clone_from(port),
            TransportConfig::Udp(udp) => udp.address = port.parse()?,
        }
    }

    if let Some(baud) = args.baud {
        if let TransportConfig::Usb(usb) = &mut config.transport {
            usb.baud_rate = baud;
        }
    }

    if let Some(server_port) = args.server_port {
        config.server.port = server_port;
    }

    Ok(())
}

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
mod tests {
    use super::*;
    use star_adventurer_gti::{TransportConfig, UdpConfig};

    fn args() -> Args {
        Args {
            command: None,
            config: None,
            transport: None,
            port: None,
            baud: None,
            server_port: None,
            log_level: Level::INFO,
            service: false,
        }
    }

    #[test]
    fn parse_log_level_accepts_canonical_levels() {
        // The clap value-parser only forwards canonical strings.
        // Non-strings reach via tests below.
        assert_eq!(parse_log_level("trace").unwrap(), Level::TRACE);
        assert_eq!(parse_log_level("debug").unwrap(), Level::DEBUG);
        assert_eq!(parse_log_level("info").unwrap(), Level::INFO);
        assert_eq!(parse_log_level("warn").unwrap(), Level::WARN);
        assert_eq!(parse_log_level("error").unwrap(), Level::ERROR);
    }

    #[test]
    fn parse_log_level_rejects_invalid() {
        let err = parse_log_level("not-a-level").unwrap_err();
        assert!(err.contains("not-a-level"));
        assert!(err.contains("trace, debug, info, warn, error"));
    }

    #[test]
    fn apply_cli_overrides_no_args_leaves_config_untouched() {
        let mut cfg = Config::default();
        let baseline_port = cfg.server.port;
        apply_cli_overrides(&mut cfg, &args()).unwrap();
        assert_eq!(cfg.server.port, baseline_port);
    }

    #[test]
    fn apply_cli_overrides_switches_transport_kind_usb_to_udp() {
        let mut cfg = Config::default();
        // Default is USB; switch to UDP.
        let mut a = args();
        a.transport = Some("udp".into());
        apply_cli_overrides(&mut cfg, &a).unwrap();
        assert!(matches!(cfg.transport, TransportConfig::Udp(_)));
    }

    #[test]
    fn apply_cli_overrides_keeps_existing_usb_block_when_transport_is_usb() {
        // If the config already has a USB block, --transport=usb must
        // not stomp on its tweaked fields.
        let mut cfg = Config::default();
        if let TransportConfig::Usb(usb) = &mut cfg.transport {
            usb.port = "/dev/ttyUSB-tweaked".into();
        }
        let mut a = args();
        a.transport = Some("usb".into());
        apply_cli_overrides(&mut cfg, &a).unwrap();
        if let TransportConfig::Usb(usb) = &cfg.transport {
            assert_eq!(usb.port, "/dev/ttyUSB-tweaked");
        } else {
            panic!("transport must remain Usb");
        }
    }

    #[test]
    fn apply_cli_overrides_rejects_unknown_transport_kind() {
        let mut cfg = Config::default();
        let mut a = args();
        a.transport = Some("bluetooth".into());
        let err = apply_cli_overrides(&mut cfg, &a).unwrap_err();
        assert!(err.to_string().contains("bluetooth"));
    }

    #[test]
    fn apply_cli_overrides_port_sets_usb_port_path() {
        let mut cfg = Config::default();
        let mut a = args();
        a.port = Some("/dev/ttyACM7".into());
        apply_cli_overrides(&mut cfg, &a).unwrap();
        if let TransportConfig::Usb(usb) = &cfg.transport {
            assert_eq!(usb.port, "/dev/ttyACM7");
        } else {
            panic!("transport must remain Usb");
        }
    }

    #[test]
    fn apply_cli_overrides_port_sets_udp_address_when_udp_transport() {
        let mut cfg = Config {
            transport: TransportConfig::Udp(UdpConfig::default()),
            ..Config::default()
        };
        let mut a = args();
        // UdpConfig.address is an IpAddr -- bare IP, no port suffix.
        a.port = Some("10.0.0.1".into());
        apply_cli_overrides(&mut cfg, &a).unwrap();
        if let TransportConfig::Udp(udp) = &cfg.transport {
            assert_eq!(udp.address.to_string(), "10.0.0.1");
        } else {
            panic!("transport must remain Udp");
        }
    }

    #[test]
    fn apply_cli_overrides_port_with_invalid_udp_address_returns_err() {
        let mut cfg = Config {
            transport: TransportConfig::Udp(UdpConfig::default()),
            ..Config::default()
        };
        let mut a = args();
        a.port = Some("not-an-address".into());
        assert!(apply_cli_overrides(&mut cfg, &a).is_err());
    }

    #[test]
    fn apply_cli_overrides_baud_only_applies_to_usb() {
        let mut cfg = Config::default();
        let mut a = args();
        a.baud = Some(57600);
        apply_cli_overrides(&mut cfg, &a).unwrap();
        if let TransportConfig::Usb(usb) = &cfg.transport {
            assert_eq!(usb.baud_rate, 57600);
        } else {
            panic!("transport must remain Usb");
        }

        // Same flag against a UDP transport must be a no-op (not an error).
        let mut udp_cfg = Config {
            transport: TransportConfig::Udp(UdpConfig::default()),
            ..Config::default()
        };
        apply_cli_overrides(&mut udp_cfg, &a).unwrap();
        assert!(matches!(udp_cfg.transport, TransportConfig::Udp(_)));
    }

    #[test]
    fn apply_cli_overrides_server_port_sets_server_port() {
        let mut cfg = Config::default();
        let mut a = args();
        a.server_port = Some(54321);
        apply_cli_overrides(&mut cfg, &a).unwrap();
        assert_eq!(cfg.server.port, 54321);
    }
}
