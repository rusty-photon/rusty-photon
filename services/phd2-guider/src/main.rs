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

use clap::{Parser, Subcommand};
use phd2_guider::{
    load_config, Config, Phd2Client, Phd2Config, Phd2Event, Rect, ServerBuilder, SettleParams,
};
use rusty_photon_service_lifecycle::{ServiceResult, ServiceRunner, Shutdown};
use std::path::PathBuf;
use std::time::Duration;
use tracing::{debug, info, Level};

fn parse_duration(s: &str) -> Result<Duration, humantime::DurationError> {
    humantime::parse_duration(s)
}

#[derive(Parser)]
#[command(name = "phd2-guider")]
#[command(about = "PHD2 guider client for Rusty Photon")]
struct Args {
    /// Path to configuration file
    #[arg(short, long)]
    config: Option<PathBuf>,

    /// PHD2 host address (default: localhost)
    #[arg(long)]
    host: Option<String>,

    /// PHD2 port (default: 4400)
    #[arg(long)]
    port: Option<u16>,

    /// Log level
    #[arg(short, long, default_value = "info", value_parser = clap::value_parser!(Level))]
    log_level: Level,

    /// Run as a Windows service (used by the service control manager).
    /// No-op on non-Windows targets.
    #[arg(long, hide = true)]
    service: bool,

    /// Subcommand; running with none starts the HTTP service (the
    /// packaged systemd unit invokes the bare binary).
    #[command(subcommand)]
    command: Option<Commands>,
}

#[derive(Subcommand)]
enum Commands {
    /// Run the rp-managed guider HTTP service — the default when no
    /// subcommand is given (see docs/services/phd2-guider.md § "HTTP
    /// Service Mode")
    Serve,

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

    /// Connect to PHD2 and show status
    Status,

    /// Connect to PHD2 and monitor events
    Monitor,

    /// Connect equipment in PHD2
    Connect,

    /// Disconnect equipment in PHD2
    Disconnect,

    /// List available profiles
    Profiles,

    /// Start guiding
    Guide {
        /// Recalibrate before guiding
        #[arg(long)]
        recalibrate: bool,

        /// Settling pixels threshold (default: 0.5)
        #[arg(long)]
        settle_pixels: Option<f64>,

        /// Settling time, e.g. "10s", "500ms" (default: 10s)
        #[arg(long, value_parser = parse_duration)]
        settle_time: Option<Duration>,

        /// Settling timeout, e.g. "60s", "1m30s" (default: 60s)
        #[arg(long, value_parser = parse_duration)]
        settle_timeout: Option<Duration>,

        /// Region of interest: x,y,width,height (e.g., "100,100,200,200")
        #[arg(long)]
        roi: Option<String>,
    },

    /// Stop guiding (continues looping)
    StopGuiding,

    /// Stop all capture and guiding
    StopCapture,

    /// Start looping exposures
    Loop,

    /// Pause guiding
    Pause {
        /// Full pause (stop looping entirely)
        #[arg(long)]
        full: bool,
    },

    /// Resume guiding after pause
    Resume,

    /// Check if guiding is paused
    IsPaused,

    /// Dither the guide position
    Dither {
        /// Dither amount in pixels
        #[arg(default_value = "5.0")]
        amount: f64,

        /// Only dither in RA axis
        #[arg(long)]
        ra_only: bool,

        /// Settling pixels threshold (default: 0.5)
        #[arg(long)]
        settle_pixels: Option<f64>,

        /// Settling time, e.g. "10s", "500ms" (default: 10s)
        #[arg(long, value_parser = parse_duration)]
        settle_time: Option<Duration>,

        /// Settling timeout, e.g. "60s", "1m30s" (default: 60s)
        #[arg(long, value_parser = parse_duration)]
        settle_timeout: Option<Duration>,
    },
}

fn main() -> ServiceResult {
    let args = Args::parse();

    if let Some(Commands::Doctor { config, json }) = args.command {
        phd2_guider::doctor::run(config, json);
    }

    // In Windows SCM service mode logs go to the rolling file under
    // %PROGRAMDATA%\rusty-photon\logs\; hold the guard until process exit so
    // the final lines flush on SCM Stop. Console mode logs to stderr as before.
    let _tracing_guard = rusty_photon_service_lifecycle::init_service_tracing(
        "phd2-guider",
        args.log_level,
        args.service,
    );

    debug!(
        "Parsed command line arguments: host={:?}, port={:?}, log_level={:?}",
        args.host, args.port, args.log_level
    );

    ServiceRunner::new("phd2-guider")
        .scm_mode(args.service)
        .run(move |shutdown| async move {
            // No subcommand = serve (the packaged systemd unit invokes the
            // bare binary).
            let command = args.command.unwrap_or(Commands::Serve);

            let config = resolve_config(
                &command,
                args.config.as_deref(),
                args.host.clone(),
                args.port,
            )?;

            if matches!(command, Commands::Serve) {
                return run_serve(config, shutdown).await;
            }

            let client = Phd2Client::new(config.phd2);

            match command {
                // `Serve` is handled by the early return above and
                // `Doctor` by the early dispatch in `main` (which exits
                // the process); both kept for match exhaustiveness.
                Commands::Serve | Commands::Doctor { .. } => {}
                Commands::Status => run_status(&client).await?,
                Commands::Monitor => run_monitor(&client, shutdown).await?,
                Commands::Connect => run_connect(&client).await?,
                Commands::Disconnect => run_disconnect(&client).await?,
                Commands::Profiles => run_profiles(&client).await?,
                Commands::Guide {
                    recalibrate,
                    settle_pixels,
                    settle_time,
                    settle_timeout,
                    roi,
                } => {
                    run_guide(
                        &client,
                        recalibrate,
                        settle_pixels,
                        settle_time,
                        settle_timeout,
                        roi,
                    )
                    .await?;
                }
                Commands::StopGuiding => run_stop_guiding(&client).await?,
                Commands::StopCapture => run_stop_capture(&client).await?,
                Commands::Loop => run_loop(&client).await?,
                Commands::Pause { full } => run_pause(&client, full).await?,
                Commands::Resume => run_resume(&client).await?,
                Commands::IsPaused => run_is_paused(&client).await?,
                Commands::Dither {
                    amount,
                    ra_only,
                    settle_pixels,
                    settle_time,
                    settle_timeout,
                } => {
                    run_dither(
                        &client,
                        amount,
                        ra_only,
                        settle_pixels,
                        settle_time,
                        settle_timeout,
                    )
                    .await?;
                }
            }

            Ok(())
        })
}

/// Build configuration from CLI args or config file. An explicit
/// `--config` must load; without one, `serve` uses the platform
/// config path (systemd passes no arguments) — materializing the
/// default config there on first start when no `--host`/`--port`
/// override is in play — and every remaining mode falls back to
/// defaults with the `--host`/`--port` flags applied, writing nothing.
fn resolve_config(
    command: &Commands,
    config_arg: Option<&std::path::Path>,
    host: Option<String>,
    port: Option<u16>,
) -> Result<Config, Box<dyn std::error::Error + Send + Sync>> {
    let is_serve = matches!(command, Commands::Serve);
    if let Some(config_path) = config_arg {
        debug!("Loading configuration from {:?}", config_path);
        return load_config(config_path);
    }
    if is_serve && host.is_none() && port.is_none() {
        let path = rusty_photon_config::resolve_and_init(
            "phd2-guider",
            None,
            &serde_json::to_value(Config::default())?,
            &[],
        )?;
        debug!("Loading configuration from default path {:?}", path);
        return load_config(&path);
    }
    let default_path = is_serve
        .then(|| rusty_photon_config::resolve_config_path("phd2-guider", None).ok())
        .flatten()
        .filter(|p| p.exists());
    if let Some(path) = default_path {
        debug!("Loading configuration from default path {:?}", path);
        return load_config(&path);
    }
    let mut phd2 = Phd2Config::default();
    if let Some(host) = host {
        phd2.host = host;
    }
    if let Some(port) = port {
        phd2.port = port;
    }
    Ok(Config {
        phd2,
        ..Default::default()
    })
}

/// Run the rp-managed guider HTTP service until shutdown.
///
/// Prints `bound_addr=<host>:<port>` to stdout once the listener is
/// bound (the `bdd-infra::ServiceHandle` port-discovery convention).
async fn run_serve(
    config: Config,
    shutdown: Shutdown,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let bound = ServerBuilder::new().with_config(config).build().await?;
    // Parsed by test harnesses; keep the exact format.
    // Console mode only: stdout is a dead handle under the Windows SCM,
    // and the only stdout consumer (bdd-infra's port parser) never runs
    // services with --service.
    if !rusty_photon_service_lifecycle::is_scm_service() {
        println!("bound_addr={}", bound.listen_addr());
    }
    info!("guider service listening on {}", bound.listen_addr());
    bound.start(shutdown.cancelled()).await?;
    Ok(())
}

async fn run_status(client: &Phd2Client) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    info!("Connecting to PHD2...");
    client.connect().await?;

    // Wait a moment for the Version event
    tokio::time::sleep(std::time::Duration::from_millis(500)).await;

    if let Some(version) = client.get_phd2_version().await {
        info!("PHD2 Version: {}", version);
    }

    let state = client.get_app_state().await?;
    info!("PHD2 State: {}", state);

    let connected = client.is_equipment_connected().await?;
    info!("Equipment connected: {}", connected);

    if connected {
        let profile = client.get_current_profile().await?;
        info!("Current profile: {} (id: {})", profile.name, profile.id);
    }

    client.disconnect().await?;
    Ok(())
}

async fn run_monitor(
    client: &Phd2Client,
    shutdown: Shutdown,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    info!("Connecting to PHD2...");
    client.connect().await?;

    #[cfg(unix)]
    info!("Monitoring PHD2 events (press Ctrl+C or send SIGTERM to stop)...");
    #[cfg(not(unix))]
    info!("Monitoring PHD2 events (press Ctrl+C to stop)...");

    let mut receiver = client.subscribe();
    let token = shutdown.token();

    loop {
        tokio::select! {
            event = receiver.recv() => {
                match event {
                    Ok(event) => {
                        print_event(&event);
                    }
                    Err(e) => {
                        debug!("Event receiver error: {}", e);
                        break;
                    }
                }
            }
            () = token.cancelled() => {
                info!("Shutting down...");
                break;
            }
        }
    }

    client.disconnect().await?;
    Ok(())
}

fn print_event(event: &Phd2Event) {
    match event {
        Phd2Event::Version { phd_version, .. } => {
            info!("Event: Version - PHD2 {}", phd_version);
        }
        Phd2Event::AppState { state } => {
            info!("Event: AppState - {}", state);
        }
        Phd2Event::GuideStep(stats) => {
            info!(
                "Event: GuideStep - Frame {} dx={:.2} dy={:.2} SNR={:.1}",
                stats.frame,
                stats.dx,
                stats.dy,
                stats.snr.unwrap_or(0.0)
            );
        }
        Phd2Event::StarSelected { x, y } => {
            info!("Event: StarSelected - ({:.1}, {:.1})", x, y);
        }
        Phd2Event::StarLost { status, .. } => {
            info!("Event: StarLost - {}", status);
        }
        Phd2Event::SettleDone { status, error } => {
            if *status == 0 {
                info!("Event: SettleDone - Success");
            } else {
                info!(
                    "Event: SettleDone - Failed: {}",
                    error.as_deref().unwrap_or("unknown")
                );
            }
        }
        Phd2Event::GuidingDithered { dx, dy } => {
            info!("Event: GuidingDithered - dx={:.2} dy={:.2}", dx, dy);
        }
        Phd2Event::Calibrating { step, state, .. } => {
            info!("Event: Calibrating - step {} ({})", step, state);
        }
        Phd2Event::CalibrationComplete { mount } => {
            info!("Event: CalibrationComplete - {}", mount);
        }
        Phd2Event::CalibrationFailed { reason } => {
            info!("Event: CalibrationFailed - {}", reason);
        }
        Phd2Event::LoopingExposures { frame } => {
            info!("Event: LoopingExposures - Frame {}", frame);
        }
        Phd2Event::LoopingExposuresStopped => {
            info!("Event: LoopingExposuresStopped");
        }
        Phd2Event::Paused => {
            info!("Event: Paused");
        }
        Phd2Event::Resumed => {
            info!("Event: Resumed");
        }
        Phd2Event::Alert { msg, alert_type } => {
            info!("Event: Alert [{}] - {}", alert_type, msg);
        }
        Phd2Event::StartGuiding => {
            info!("Event: StartGuiding");
        }
        Phd2Event::GuidingStopped => {
            info!("Event: GuidingStopped");
        }
        Phd2Event::Settling { distance, time, .. } => {
            info!(
                "Event: Settling - distance={:.2} time={:.1}s",
                distance,
                time.as_secs_f64()
            );
        }
        _ => {
            debug!("Event: {:?}", event);
        }
    }
}

async fn run_connect(client: &Phd2Client) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    info!("Connecting to PHD2...");
    client.connect().await?;

    info!("Connecting equipment...");
    client.connect_equipment().await?;
    info!("Equipment connected successfully");

    client.disconnect().await?;
    Ok(())
}

async fn run_disconnect(
    client: &Phd2Client,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    info!("Connecting to PHD2...");
    client.connect().await?;

    info!("Disconnecting equipment...");
    client.disconnect_equipment().await?;
    info!("Equipment disconnected successfully");

    client.disconnect().await?;
    Ok(())
}

async fn run_profiles(client: &Phd2Client) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    info!("Connecting to PHD2...");
    client.connect().await?;

    let profiles = client.get_profiles().await?;
    info!("Available profiles:");
    for profile in &profiles {
        info!("  [{}] {}", profile.id, profile.name);
    }

    let current = client.get_current_profile().await?;
    info!("Current profile: {} (id: {})", current.name, current.id);

    client.disconnect().await?;
    Ok(())
}

// ============================================================================
// Guiding Control Commands
// ============================================================================

fn parse_roi(roi_str: &str) -> Result<Rect, Box<dyn std::error::Error + Send + Sync>> {
    let parts: Vec<&str> = roi_str.split(',').collect();
    let [x, y, width, height] = parts.as_slice() else {
        return Err("ROI must be in format: x,y,width,height".into());
    };
    Ok(Rect::new(
        x.trim().parse()?,
        y.trim().parse()?,
        width.trim().parse()?,
        height.trim().parse()?,
    ))
}

async fn run_guide(
    client: &Phd2Client,
    recalibrate: bool,
    settle_pixels: Option<f64>,
    settle_time: Option<Duration>,
    settle_timeout: Option<Duration>,
    roi: Option<String>,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    info!("Connecting to PHD2...");
    client.connect().await?;

    let settle = SettleParams {
        pixels: settle_pixels.unwrap_or(0.5),
        time: settle_time.unwrap_or(Duration::from_secs(10)),
        timeout: settle_timeout.unwrap_or(Duration::from_mins(1)),
    };

    let roi_rect = match roi {
        Some(s) => Some(parse_roi(&s)?),
        None => None,
    };

    info!(
        "Starting guiding (recalibrate={}, settle: pixels={}, time={:?}, timeout={:?})",
        recalibrate, settle.pixels, settle.time, settle.timeout
    );

    client.start_guiding(&settle, recalibrate, roi_rect).await?;
    info!("Guide command sent successfully");

    client.disconnect().await?;
    Ok(())
}

async fn run_stop_guiding(
    client: &Phd2Client,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    info!("Connecting to PHD2...");
    client.connect().await?;

    info!("Stopping guiding (continuing loop)...");
    client.stop_guiding().await?;
    info!("Stop guiding command sent successfully");

    client.disconnect().await?;
    Ok(())
}

async fn run_stop_capture(
    client: &Phd2Client,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    info!("Connecting to PHD2...");
    client.connect().await?;

    info!("Stopping capture...");
    client.stop_capture().await?;
    info!("Stop capture command sent successfully");

    client.disconnect().await?;
    Ok(())
}

async fn run_loop(client: &Phd2Client) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    info!("Connecting to PHD2...");
    client.connect().await?;

    info!("Starting loop...");
    client.start_loop().await?;
    info!("Loop command sent successfully");

    client.disconnect().await?;
    Ok(())
}

async fn run_pause(
    client: &Phd2Client,
    full: bool,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    info!("Connecting to PHD2...");
    client.connect().await?;

    info!("Pausing guiding (full={})...", full);
    client.pause(full).await?;
    info!("Pause command sent successfully");

    client.disconnect().await?;
    Ok(())
}

async fn run_resume(client: &Phd2Client) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    info!("Connecting to PHD2...");
    client.connect().await?;

    info!("Resuming guiding...");
    client.resume().await?;
    info!("Resume command sent successfully");

    client.disconnect().await?;
    Ok(())
}

async fn run_is_paused(
    client: &Phd2Client,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    info!("Connecting to PHD2...");
    client.connect().await?;

    let paused = client.is_paused().await?;
    info!("Guiding is paused: {}", paused);

    client.disconnect().await?;
    Ok(())
}

async fn run_dither(
    client: &Phd2Client,
    amount: f64,
    ra_only: bool,
    settle_pixels: Option<f64>,
    settle_time: Option<Duration>,
    settle_timeout: Option<Duration>,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    info!("Connecting to PHD2...");
    client.connect().await?;

    let settle = SettleParams {
        pixels: settle_pixels.unwrap_or(0.5),
        time: settle_time.unwrap_or(Duration::from_secs(10)),
        timeout: settle_timeout.unwrap_or(Duration::from_mins(1)),
    };

    info!(
        "Dithering (amount={}, ra_only={}, settle: pixels={}, time={:?}, timeout={:?})",
        amount, ra_only, settle.pixels, settle.time, settle.timeout
    );

    client.dither(amount, ra_only, &settle).await?;
    info!("Dither command sent successfully");

    client.disconnect().await?;
    Ok(())
}
