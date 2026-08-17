//! Mock PHD2 server for testing
//!
//! A simple mock PHD2 server that responds to JSON-RPC requests and
//! emits the event stream the HTTP service mode's settle wait and
//! stop poll observe.
//!
//! Usage:
//!   `mock_phd2` [--port PORT]
//!
//! Environment variables:
//!   `MOCK_PHD2_PORT` - Port to listen on (0 = auto-assign, default: 4400)
//!   `MOCK_PHD2_MODE` - Operating mode for testing different scenarios:
//!     - "normal" (default): Standard mock server behavior
//!     - "`exit_immediately"`: Exit with code 42 without starting server
//!     - "`no_listen"`: Sleep without binding to port (tests connection timeout)
//!     - "`slow_start"`: Wait 5 seconds before binding (tests startup timing)
//!     - "`shutdown_fails"`: Ignore shutdown commands (tests fallback to force kill)
//!   `MOCK_PHD2_SETTLE_MODE` - What follows a `guide`/`dither` RPC:
//!     - "`settle_ok`" (default): emit Settling, two fixed `GuideStep` events
//!       (`RADistanceRaw` +0.3/-0.3, `DECDistanceRaw` -0.4/+0.4 so the RMS is
//!       deterministic: 0.3 / 0.4 / 0.5), then SettleDone{Status: 0}
//!     - "`settle_fail"`: same lead-in, then
//!       SettleDone{Status: 1, Error: "Mock star lost"}
//!     - "`never_settle"`: emit Settling and `GuideSteps` but no `SettleDone`
//!       (drives the service's `settle_timeout` backstop)
//!   `MOCK_PHD2_STOP_MODE` - `stop_capture` behavior:
//!     - "stops" (default): application state transitions to Stopped
//!     - "`never_stops"`: state stays Guiding (drives `stop_timeout`)
//!   `MOCK_PHD2_RPC_LOG` - Path to a JSON-lines file each received
//!     {"method", "params"} is appended to (request-forwarding assertions;
//!     the `MOCK_ASTAP_ARGV_OUT` equivalent)
//!   `MOCK_PHD2_ROTATOR` - "connected" populates `get_current_equipment`'s
//!     rotator slot ({"name": "Mock Rotator", "connected": true});
//!     unset/anything else reports null (no rotator in the profile)
//!
//! Command line argument takes precedence over environment variable for port.
//! Default port is 4400 (same as PHD2).
//!
//! When binding succeeds, the actual port is printed to stdout as:
//!   `MOCK_PHD2_PORT:12345`
//! This allows tests to discover the port when using auto-assign (port 0).

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::unreachable)]
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

use std::io::{BufRead, BufReader, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

/// Application state shared across connections, mirroring the single
/// state machine inside a real PHD2 instance.
type AppState = Arc<Mutex<String>>;

fn main() {
    // Get operating mode from environment
    let mode = std::env::var("MOCK_PHD2_MODE").unwrap_or_else(|_| "normal".to_string());

    // Handle special modes that don't start the server
    match mode.as_str() {
        "exit_immediately" => {
            eprintln!("Mock PHD2: exit_immediately mode - exiting with code 42");
            std::process::exit(42);
        }
        "no_listen" => {
            eprintln!("Mock PHD2: no_listen mode - sleeping without binding port");
            // Sleep for a while to allow timeout tests
            std::thread::sleep(std::time::Duration::from_secs(30));
            std::process::exit(0);
        }
        "slow_start" => {
            eprintln!("Mock PHD2: slow_start mode - waiting 5 seconds before starting");
            std::thread::sleep(std::time::Duration::from_secs(5));
            // Continue to normal startup
        }
        "normal" | "shutdown_fails" => {
            // Continue to normal startup
        }
        _ => {
            eprintln!("Mock PHD2: unknown mode '{mode}', using normal");
        }
    }

    // Port priority: command line arg > environment variable > default (4400)
    // Port 0 means auto-assign by OS
    let requested_port = std::env::args()
        .nth(2)
        .and_then(|s| s.parse().ok())
        .or_else(|| {
            std::env::var("MOCK_PHD2_PORT")
                .ok()
                .and_then(|s| s.parse().ok())
        })
        .unwrap_or(4400u16);

    eprintln!(
        "Mock PHD2 starting on port {} (mode: {})",
        if requested_port == 0 {
            "auto".to_string()
        } else {
            requested_port.to_string()
        },
        mode
    );

    let listener = match TcpListener::bind(format!("127.0.0.1:{requested_port}")) {
        Ok(l) => l,
        Err(e) => {
            eprintln!("Failed to bind to port {requested_port}: {e}");
            std::process::exit(1);
        }
    };

    // Get the actual port (important when requested_port was 0)
    let actual_port = listener
        .local_addr()
        .expect("Failed to get local address")
        .port();

    let shutdown = Arc::new(AtomicBool::new(false));
    let app_state: AppState = Arc::new(Mutex::new("Stopped".to_string()));

    // Set a timeout so we can check shutdown flag periodically
    listener
        .set_nonblocking(true)
        .expect("Cannot set non-blocking");

    // Print actual port to stdout for test discovery (parseable format)
    println!("MOCK_PHD2_PORT:{actual_port}");

    eprintln!("Mock PHD2 listening on port {actual_port}");

    // Store mode for use in request handler
    let ignore_shutdown = mode == "shutdown_fails";

    while !shutdown.load(Ordering::Relaxed) {
        match listener.accept() {
            Ok((stream, addr)) => {
                eprintln!("Connection from {addr}");
                let shutdown_clone = shutdown.clone();
                let ignore_shutdown_clone = ignore_shutdown;
                let app_state_clone = app_state.clone();
                std::thread::spawn(move || {
                    handle_client(
                        stream,
                        &shutdown_clone,
                        ignore_shutdown_clone,
                        &app_state_clone,
                    );
                });
            }
            Err(ref e)
                if e.kind() == std::io::ErrorKind::WouldBlock
                    || e.kind() == std::io::ErrorKind::TimedOut =>
            {
                // No connection available, sleep briefly
                std::thread::sleep(std::time::Duration::from_millis(100));
            }
            Err(e) => {
                eprintln!("Accept error: {e}");
            }
        }
    }

    eprintln!("Mock PHD2 shutting down");
}

/// Write one whole line under the writer lock so event-emitter threads
/// and the response path never interleave mid-line.
fn write_line(writer: &Arc<Mutex<TcpStream>>, line: &str) -> std::io::Result<()> {
    let mut stream = writer.lock().unwrap();
    writeln!(stream, "{line}")?;
    stream.flush()
}

/// Append the received RPC to the JSON-lines log named by
/// `MOCK_PHD2_RPC_LOG`, when set. One process, many threads: a global
/// lock serializes appends.
fn log_rpc(method: &str, params: &serde_json::Value) {
    static LOG_LOCK: Mutex<()> = Mutex::new(());
    let Ok(path) = std::env::var("MOCK_PHD2_RPC_LOG") else {
        return;
    };
    let line = serde_json::json!({ "method": method, "params": params }).to_string();
    let _guard = LOG_LOCK.lock().unwrap();
    if let Ok(mut file) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)
    {
        let _ = writeln!(file, "{line}");
    }
}

/// Emit the event sequence that follows a `guide` or `dither` RPC,
/// per `MOCK_PHD2_SETTLE_MODE`. Runs on its own thread so the request
/// loop keeps answering RPCs (the service polls `get_app_state` and
/// waits for `SettleDone` concurrently).
fn emit_settle_sequence(writer: Arc<Mutex<TcpStream>>) {
    let settle_mode =
        std::env::var("MOCK_PHD2_SETTLE_MODE").unwrap_or_else(|_| "settle_ok".to_string());
    std::thread::spawn(move || {
        let pause = Duration::from_millis(30);
        std::thread::sleep(pause);
        let _ = write_line(
            &writer,
            r#"{"Event":"Settling","Distance":1.2,"Time":0.5,"SettleTime":10.0,"StarLocked":true}"#,
        );
        std::thread::sleep(pause);
        let _ = write_line(
            &writer,
            r#"{"Event":"GuideStep","Frame":1,"Time":1.0,"Mount":"Mock Mount","dx":0.1,"dy":0.1,"RADistanceRaw":0.3,"DECDistanceRaw":-0.4,"SNR":25.1,"StarMass":5340.0,"HFD":2.3}"#,
        );
        std::thread::sleep(pause);
        let _ = write_line(
            &writer,
            r#"{"Event":"GuideStep","Frame":2,"Time":2.0,"Mount":"Mock Mount","dx":0.1,"dy":0.1,"RADistanceRaw":-0.3,"DECDistanceRaw":0.4,"SNR":25.1,"StarMass":5340.0,"HFD":2.5}"#,
        );
        std::thread::sleep(pause);
        // A star-lost frame after the guide steps: exercises the
        // metrics ring's star_lost entries without perturbing the RMS
        // window (StarLost carries no distances).
        let _ = write_line(
            &writer,
            r#"{"Event":"StarLost","Frame":3,"Time":3.0,"StarMass":900.0,"SNR":3.1,"Status":"Lost"}"#,
        );
        std::thread::sleep(pause);
        match settle_mode.as_str() {
            "settle_fail" => {
                let _ = write_line(
                    &writer,
                    r#"{"Event":"SettleDone","Status":1,"Error":"Mock star lost"}"#,
                );
            }
            "never_settle" => {
                eprintln!("Mock PHD2: never_settle mode - withholding SettleDone");
            }
            _ => {
                let _ = write_line(&writer, r#"{"Event":"SettleDone","Status":0}"#);
            }
        }
    });
}

fn handle_client(
    stream: TcpStream,
    shutdown: &Arc<AtomicBool>,
    ignore_shutdown: bool,
    app_state: &AppState,
) {
    stream
        .set_read_timeout(Some(std::time::Duration::from_secs(5)))
        .ok();
    stream
        .set_write_timeout(Some(std::time::Duration::from_secs(5)))
        .ok();

    let reader = BufReader::new(stream.try_clone().unwrap());
    let writer = Arc::new(Mutex::new(stream));

    // Send Version event on connect
    let version_event =
        r#"{"Event":"Version","PHDVersion":"2.6.11-mock","PHDSubver":"test","MsgVersion":1}"#;
    if write_line(&writer, version_event).is_err() {
        return;
    }

    eprintln!("Sent version event");

    for line in reader.lines() {
        match line {
            Ok(request) => {
                if request.is_empty() {
                    continue;
                }

                eprintln!("Received: {request}");

                let response =
                    handle_request(&request, shutdown, ignore_shutdown, app_state, &writer);
                eprintln!("Sending: {response}");

                if write_line(&writer, &response).is_err() {
                    break;
                }

                // Check if we should shutdown
                if shutdown.load(Ordering::Relaxed) {
                    break;
                }
            }
            Err(ref e)
                if e.kind() == std::io::ErrorKind::WouldBlock
                    || e.kind() == std::io::ErrorKind::TimedOut =>
            {
                // Timeout, check shutdown flag
                if shutdown.load(Ordering::Relaxed) {
                    break;
                }
            }
            Err(_) => {
                break;
            }
        }
    }

    eprintln!("Client disconnected");
}

fn handle_request(
    request: &str,
    shutdown: &Arc<AtomicBool>,
    ignore_shutdown: bool,
    app_state: &AppState,
    writer: &Arc<Mutex<TcpStream>>,
) -> String {
    // Parse JSON-RPC request
    let req: serde_json::Value = match serde_json::from_str(request) {
        Ok(v) => v,
        Err(_) => {
            return r#"{"jsonrpc":"2.0","error":{"code":-32700,"message":"Parse error"},"id":null}"#
                .to_string();
        }
    };

    let id = req.get("id").cloned().unwrap_or(serde_json::Value::Null);
    let method = req.get("method").and_then(|m| m.as_str()).unwrap_or("");
    let params = req
        .get("params")
        .cloned()
        .unwrap_or(serde_json::Value::Null);
    log_rpc(method, &params);

    // Mock responses for different methods
    let result = match method {
        "get_app_state" => serde_json::json!(app_state.lock().unwrap().clone()),
        // Boolean getters the mock reports as "off" / "not yet".
        "get_connected" | "get_use_subframes" | "get_calibrated" | "get_paused" => {
            serde_json::json!(false)
        }
        // Setters and one-shot commands with no state to update here —
        // the mock acknowledges with the PHD2 success code 0.
        "set_connected"
        | "set_profile"
        | "set_exposure"
        | "clear_calibration"
        | "flip_calibration"
        | "set_lock_position"
        | "find_star"
        | "set_paused"
        | "set_algo_param"
        | "set_cooler_state"
        | "capture_single_frame" => {
            serde_json::json!(0)
        }
        "get_profiles" => serde_json::json!([
            {"id": 1, "name": "Mock Profile"}
        ]),
        "get_profile" => serde_json::json!({"id": 1, "name": "Mock Profile"}),
        "get_current_equipment" => {
            // MOCK_PHD2_ROTATOR=connected populates the rotator slot —
            // the branch rp's rotate-while-guiding ladder takes when
            // PHD2 adjusts calibration for rotation on its own.
            let rotator = if std::env::var("MOCK_PHD2_ROTATOR").as_deref() == Ok("connected") {
                serde_json::json!({"name": "Mock Rotator", "connected": true})
            } else {
                serde_json::Value::Null
            };
            serde_json::json!({
                "camera": {"name": "Mock Camera", "connected": false},
                "mount": {"name": "Mock Mount", "connected": false},
                "aux_mount": null,
                "AO": null,
                "rotator": rotator
            })
        }
        "get_exposure" => serde_json::json!(1000),
        "get_exposure_durations" => serde_json::json!([100, 200, 500, 1000, 2000, 3000]),
        "get_camera_frame_size" => serde_json::json!([640, 480]),
        "get_calibration_data" => serde_json::json!({
            "calibrated": false,
            "xAngle": 0.0,
            "xRate": 0.0,
            "xParity": "+",
            "yAngle": 0.0,
            "yRate": 0.0,
            "yParity": "+"
        }),
        "get_lock_position" => serde_json::json!(null),
        "guide" => {
            *app_state.lock().unwrap() = "Guiding".to_string();
            emit_settle_sequence(writer.clone());
            serde_json::json!(0)
        }
        "loop" => {
            *app_state.lock().unwrap() = "Looping".to_string();
            serde_json::json!(0)
        }
        "stop_capture" => {
            let stop_mode =
                std::env::var("MOCK_PHD2_STOP_MODE").unwrap_or_else(|_| "stops".to_string());
            if stop_mode == "never_stops" {
                eprintln!("Mock PHD2: never_stops mode - state unchanged");
            } else {
                *app_state.lock().unwrap() = "Stopped".to_string();
            }
            serde_json::json!(0)
        }
        "dither" => {
            emit_settle_sequence(writer.clone());
            serde_json::json!(0)
        }
        "get_algo_param_names" => serde_json::json!(["Aggressiveness", "MinMove"]),
        "get_algo_param" => serde_json::json!(0.5),
        "get_ccd_temperature" => serde_json::json!(20.0),
        "get_cooler_status" => serde_json::json!({
            "temperature": 20.0,
            "coolerOn": false
        }),
        "get_star_image" => serde_json::json!({
            "frame": 1,
            "width": 32,
            "height": 32,
            "star_pos": [16.0, 16.0],
            "pixels": "AAAA"
        }),
        "save_image" => serde_json::json!("/tmp/mock_image.fits"),
        "shutdown" => {
            if ignore_shutdown {
                // Report success but don't actually shut down.
                eprintln!("Shutdown requested but ignored (shutdown_fails mode)");
            } else {
                eprintln!("Shutdown requested");
                shutdown.store(true, Ordering::Relaxed);
            }
            serde_json::json!(0)
        }
        _ => {
            return format!(
                r#"{{"jsonrpc":"2.0","error":{{"code":-32601,"message":"Method not found: {method}"}},"id":{id}}}"#
            );
        }
    };

    format!(r#"{{"jsonrpc":"2.0","result":{result},"id":{id}}}"#)
}
