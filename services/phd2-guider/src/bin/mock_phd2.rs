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
//!   `MOCK_PHD2_IMAGE_DIR` - Absolute directory `save_image` writes its FITS
//!     file to. Each call writes a new file and replies
//!     {"filename": "<full path>"}, as PHD2 does; the file is left for the
//!     client to remove. There is no default: unset, not UTF-8 or not
//!     absolute, `save_image` writes nothing and answers PHD2's "error
//!     saving image"
//!   `MOCK_PHD2_VERSION` - the `PHDVersion` the `Version` event reports
//!     (default "2.6.11-mock")
//!   `MOCK_PHD2_FRAME_SIZE` - the camera's frame size as `<width>x<height>`
//!     (default "640x480"): `get_camera_frame_size`'s answer and the size
//!     of the frames `capture_single_frame` saves
//!   `MOCK_PHD2_FRAME_SIZE_KNOWN` - when `get_camera_frame_size` knows it:
//!     - "`at_connect`" (default): from the start, as PHD2's QHY and ZWO
//!       drivers do
//!     - "`after_first_frame`": `[0, 0]` until a frame has been saved, as
//!       PHD2's Simulator and INDI cameras do
//!   `MOCK_PHD2_CAMERA` - "disconnected" makes `get_camera_frame_size`
//!     answer PHD2's "camera not connected" and `capture_single_frame`
//!     answer with PHD2 2.6.14's reply carrying two `error` members
//!   `MOCK_PHD2_CAPTURE_MODE` - how a saved `capture_single_frame` frame
//!     ends (PHD2 2.6.14's `save`/`path` capture; see `capture_single_frame`):
//!     - "completes" (default): the frame is written at the path, then
//!       `SingleFrameComplete {Success: true, Path}`
//!     - "fails": no file, `SingleFrameComplete {Success: false, Error:
//!       "Mock camera error", Path}`
//!     - "`never_reports`": no file and no event; the capture ends
//!       silently (drives the facade's deadline)
//!
//! As in PHD2, `SingleFrameComplete` goes to every connected client.
//! One test hook is not a PHD2 method: `mock_report_frame {"path"}`
//! sends every client a successful `SingleFrameComplete` for `path`, a
//! frame PHD2 finished on its own (one a crashed guider started).
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

use std::io::{BufRead, BufReader, Write};
use std::net::{TcpListener, TcpStream};
use std::path::Path;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use rp_fits::writer::write_u16_image;

/// Application state shared across connections, mirroring the single
/// state machine inside a real PHD2 instance.
type AppState = Arc<Mutex<String>>;

/// The single frame in flight, if any: PHD2 has one camera, so one
/// frame at a time across every connection. Setting the flag asks the
/// frame's thread to end it early, as `stop_capture` does in PHD2.
static FRAME_IN_FLIGHT: Mutex<Option<Arc<AtomicBool>>> = Mutex::new(None);

/// Whether the camera has saved a frame, which tells a camera whose
/// size is known only after its first frame what to answer.
static FRAME_TAKEN: AtomicBool = AtomicBool::new(false);

/// Every connected client's writer. PHD2 sends `SingleFrameComplete` to
/// every event-server client, not only the one that asked for the frame.
static CLIENTS: Mutex<Vec<Arc<Mutex<TcpStream>>>> = Mutex::new(Vec::new());

/// A client's place in [`CLIENTS`], given up when its connection ends.
struct Registered(Arc<Mutex<TcpStream>>);

impl Registered {
    fn new(writer: &Arc<Mutex<TcpStream>>) -> Self {
        CLIENTS.lock().unwrap().push(Arc::clone(writer));
        Self(Arc::clone(writer))
    }
}

impl Drop for Registered {
    fn drop(&mut self) {
        CLIENTS
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .retain(|writer| !Arc::ptr_eq(writer, &self.0));
    }
}

/// Write one line to every connected client; a client gone away is
/// skipped.
fn broadcast(line: &str) {
    let clients = CLIENTS.lock().unwrap().clone();
    for writer in &clients {
        let _ = write_line(writer, line);
    }
}

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
                let Some(client) = accept_client(stream) else {
                    continue;
                };
                let shutdown_clone = shutdown.clone();
                let ignore_shutdown_clone = ignore_shutdown;
                let app_state_clone = app_state.clone();
                std::thread::spawn(move || {
                    handle_client(
                        client,
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
        // A star-lost frame after the guide steps: exercises the
        // metrics ring's star_lost entries without perturbing the RMS
        // window (StarLost carries no distances).
        //
        // No pause before this frame or before the SettleDone below:
        // the last frames and the settle verdict reach the service in
        // one burst, as they do from a real PHD2 that settles on a
        // guide step. A pause here would give the service's event pump
        // a head start the wire does not guarantee, and the scenarios
        // asserting the settled snapshot would stop covering the
        // ordering they exist to pin down.
        let _ = write_line(
            &writer,
            r#"{"Event":"StarLost","Frame":3,"Time":3.0,"StarMass":900.0,"SNR":3.1,"Status":"Lost"}"#,
        );
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

/// Geometry of the frame `save_image` writes.
const SAVED_FRAME_WIDTH: usize = 8;
const SAVED_FRAME_HEIGHT: usize = 6;
const SAVED_FRAME_PIXELS: usize = SAVED_FRAME_WIDTH * SAVED_FRAME_HEIGHT;
/// Step of the saved frame's row-major ramp: pixel `i` is `i × 1365`, so
/// the 48 values span 0..=64155 and cross the signed midpoint that the
/// `BZERO = 32768` encoding of unsigned 16-bit pixels pivots on.
const SAVED_FRAME_STEP: usize = 1365;

/// Answer `save_image` in PHD2's wire format: write the current frame to
/// a new FITS file and return its full path. The frame is a fixed 8×6
/// unsigned 16-bit ramp (primary HDU, `BITPIX = 16`, `BZERO = 32768`).
/// The mock always has this frame to save: it does not model which
/// exposure a frame came from, nor PHD2's `no image available` (code 2)
/// before the first one. For the same reason the header carries none of
/// the time cards real PHD2's `usImage::Save` writes (`EXPOSURE`, `DATE`,
/// `DATE-OBS`): their values depend on when an exposure ran. A completion
/// watermark that compares `DATE-OBS` (option 3 of D9 in
/// `docs/plans/device-claims-and-phd2-camera.md`) has to add them here,
/// together with an exposure model.
///
/// The file goes to `MOCK_PHD2_IMAGE_DIR` and is left there: as with
/// PHD2, removing it is the client's job. There is no default
/// directory, so no run can scatter files into a shared one. The call
/// fails, leaving nothing behind, when the variable is unset, not UTF-8
/// (the reply carries the path as a JSON string), or not absolute. The
/// absolute-path check refuses both an empty and a relative value; without
/// it, an empty value would become the filesystem root once the separator
/// is appended, and a relative one would land in the mock's working
/// directory.
/// The reply is the variable's own text plus an ASCII name, so it is
/// never converted back from a path.
///
/// PHD2's file name depends on its platform. It comes from
/// `wxFileName::CreateTempFileName(<dir>/save_image_)`, whose
/// implementation (`wxCreateTempImpl` in wxWidgets'
/// `src/common/filename.cpp`) branches on the OS. On Unix it appends six
/// `mkstemp` characters: `<dir>/save_image_XXXXXX`, no extension. On
/// Windows it calls `GetTempFileName`, which Microsoft documents as
/// keeping only the first three characters of the prefix and producing
/// `<path>\<pre><uuuu>.TMP`: `<dir>\sav<hex>.TMP`. The mock copies the
/// Unix form only (`save_image_<pid>_<n>`, no extension). A client must
/// take the name from the reply and key on neither the `save_image_`
/// prefix nor a missing extension.
///
/// The file's life is an approximation of PHD2's. PHD2 reserves the name
/// (`mkstemp`, or `GetTempFileName` on Windows), then saves through
/// cfitsio with the clobber prefix `!`, which deletes that reserved file
/// and creates a new one under the same name. The mock creates the file
/// once and writes into it, so it never shows the moment the name is
/// missing. If the write fails it removes the file, as PHD2 removes the
/// file of a failed save. Names are the process id plus a per-process
/// counter. Creation refuses a name that already exists, so one left by
/// an earlier run with the same process id is skipped for the next, never
/// overwritten, as `mkstemp` skips a taken name.
fn save_image() -> Result<String, String> {
    static NEXT_IMAGE: AtomicU64 = AtomicU64::new(0);

    let mut prefix = std::env::var("MOCK_PHD2_IMAGE_DIR")
        .map_err(|e| format!("MOCK_PHD2_IMAGE_DIR: {e}, so there is nowhere to save"))?;
    if !Path::new(&prefix).is_absolute() {
        return Err(format!(
            "MOCK_PHD2_IMAGE_DIR must be an absolute directory, not \"{prefix}\""
        ));
    }
    if !prefix.ends_with(std::path::is_separator) {
        prefix.push(std::path::MAIN_SEPARATOR);
    }

    let pixels: Vec<u16> = (0..=u16::MAX)
        .step_by(SAVED_FRAME_STEP)
        .take(SAVED_FRAME_PIXELS)
        .collect();
    let (filename, mut file) = loop {
        let serial = NEXT_IMAGE.fetch_add(1, Ordering::Relaxed);
        let filename = format!("{prefix}save_image_{}_{serial}", std::process::id());
        match std::fs::File::create_new(&filename) {
            Ok(file) => break (filename, file),
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {}
            Err(e) => return Err(format!("cannot create {filename}: {e}")),
        }
    };
    if let Err(write_err) = write_u16_image(
        &mut file,
        &pixels,
        SAVED_FRAME_WIDTH,
        SAVED_FRAME_HEIGHT,
        &[],
    ) {
        drop(file);
        if let Err(e) = std::fs::remove_file(&filename) {
            eprintln!("Mock PHD2: cannot remove {filename}: {e}");
        }
        return Err(format!("cannot write {filename}: {write_err}"));
    }
    Ok(filename)
}

/// Test hook, not a PHD2 method: report a frame PHD2 finished on its
/// own (one a crashed guider started), to every client.
fn report_frame(params: &serde_json::Value) -> serde_json::Value {
    let path = params.get("path").cloned().unwrap_or_default();
    broadcast(
        &serde_json::json!({
            "Event": "SingleFrameComplete",
            "Success": true,
            "Path": path,
        })
        .to_string(),
    );
    serde_json::json!(0)
}

/// `MOCK_PHD2_CAMERA=disconnected`.
fn camera_disconnected() -> bool {
    std::env::var("MOCK_PHD2_CAMERA").as_deref() == Ok("disconnected")
}

/// `get_camera_frame_size`'s answer: the camera's frame size, or
/// `[0, 0]` before its first frame under
/// `MOCK_PHD2_FRAME_SIZE_KNOWN=after_first_frame`.
fn reported_frame_size() -> serde_json::Value {
    let size_after_first_frame =
        std::env::var("MOCK_PHD2_FRAME_SIZE_KNOWN").as_deref() == Ok("after_first_frame");
    if size_after_first_frame && !FRAME_TAKEN.load(Ordering::Relaxed) {
        return serde_json::json!([0, 0]);
    }
    let (width, height) = frame_size();
    serde_json::json!([width, height])
}

/// The camera's frame size, from `MOCK_PHD2_FRAME_SIZE` (`<w>x<h>`),
/// 640 x 480 by default.
fn frame_size() -> (usize, usize) {
    std::env::var("MOCK_PHD2_FRAME_SIZE")
        .ok()
        .and_then(|size| {
            let (w, h) = size.split_once('x')?;
            Some((w.parse().ok()?, h.parse().ok()?))
        })
        .unwrap_or((640, 480))
}

/// Why `capture_single_frame` refused, rendered by the caller with the
/// request's id.
enum Refusal {
    /// An ordinary PHD2 error reply.
    Error(i32, &'static str),
    /// PHD2 2.6.14's reply with its camera disconnected: two `error`
    /// members in one object.
    CameraDisconnected,
}

impl Refusal {
    fn reply(&self, id: &serde_json::Value) -> String {
        match self {
            Self::Error(code, message) => format!(
                r#"{{"jsonrpc":"2.0","error":{},"id":{id}}}"#,
                serde_json::json!({"code": code, "message": message})
            ),
            Self::CameraDisconnected => format!(
                r#"{{"jsonrpc":"2.0","error":{{"code":1,"message":"cannot capture single frame when camera is not connected"}},"error":{{"code":2,"message":"failed to start exposure"}},"id":{id}}}"#
            ),
        }
    }
}

/// PHD2 2.6.14's `capture_single_frame`, as far as the camera facade
/// uses it (`docs/services/phd2-guider.md` § "What PHD2 does with a
/// single frame"). A call that asks for neither `save` nor `path` keeps
/// the old fixture's answer, `0` with no frame modelled. One with them
/// is checked as PHD2 checks it, refused while a frame or a loop is
/// running, and then exposed for `exposure` milliseconds on its own
/// thread. The thread ends the frame per `MOCK_PHD2_CAPTURE_MODE`, or
/// early when `stop_capture` arrives, and reports it with
/// `SingleFrameComplete` to every connected client. A completed
/// frame is the `MOCK_PHD2_FRAME_SIZE` ramp (pixel `i` is `i × 1365 mod
/// 65536`, row-major) written at `path` before the event is sent, as
/// PHD2 writes it first.
fn capture_single_frame(
    params: &serde_json::Value,
    app_state: &AppState,
) -> Result<serde_json::Value, Refusal> {
    if camera_disconnected() {
        return Err(Refusal::CameraDisconnected);
    }
    let path = params.get("path");
    let save = params.get("save");
    if path.is_none() && save.is_none() {
        return Ok(serde_json::json!(0));
    }
    let mut in_flight = FRAME_IN_FLIGHT.lock().unwrap();
    if in_flight.is_some() || *app_state.lock().unwrap() != "Stopped" {
        return Err(Refusal::Error(
            1,
            "cannot capture single frame when capture is currently active",
        ));
    }
    let bad_exposure = Refusal::Error(-32602, "expected exposure param");
    let exposure_ms = match params.get("exposure") {
        Some(serde_json::Value::Number(n)) => n
            .as_u64()
            .filter(|ms| (1..=600_000).contains(ms))
            .ok_or(bad_exposure)?,
        Some(_) => return Err(bad_exposure),
        None => 1000,
    };
    let path = match path {
        Some(serde_json::Value::String(p)) => Some(p.clone()),
        Some(_) => {
            return Err(Refusal::Error(
                -32602,
                "invalid path param: string expected",
            ))
        }
        None => None,
    };
    if let Some(p) = &path {
        if !Path::new(p).is_absolute() {
            return Err(Refusal::Error(
                -32602,
                "path param must be an absolute path",
            ));
        }
        if Path::new(p).exists() {
            return Err(Refusal::Error(-32602, "destination file already exists"));
        }
    }
    let save = match save {
        Some(serde_json::Value::Bool(b)) => *b,
        Some(_) => return Err(Refusal::Error(-32602, "save param must be a boolean")),
        None => path.is_some(),
    };
    if !save && path.is_some() {
        return Err(Refusal::Error(
            -32602,
            "path param not allowed when save = false",
        ));
    }

    let stop = Arc::new(AtomicBool::new(false));
    *in_flight = Some(Arc::clone(&stop));
    drop(in_flight);
    *app_state.lock().unwrap() = "Looping".to_string();
    let app_state = Arc::clone(app_state);
    std::thread::spawn(move || expose_frame(exposure_ms, path, &stop, &app_state));
    Ok(serde_json::json!(0))
}

/// One accepted frame's thread: expose until `exposure_ms` passes or
/// `stop` is set, end the frame per `MOCK_PHD2_CAPTURE_MODE`, and report
/// it to every client, as PHD2 does.
fn expose_frame(exposure_ms: u64, path: Option<String>, stop: &AtomicBool, app_state: &AppState) {
    let mode = std::env::var("MOCK_PHD2_CAPTURE_MODE").unwrap_or_else(|_| "completes".to_string());
    let deadline = std::time::Instant::now()
        .checked_add(Duration::from_millis(exposure_ms))
        .unwrap();
    while std::time::Instant::now() < deadline && !stop.load(Ordering::Relaxed) {
        std::thread::sleep(Duration::from_millis(5));
    }
    // `None`: the frame ends silently. `Some(Ok)`: saved.
    // `Some(Err)`: failed, with PHD2's error text.
    let outcome: Option<Result<(), String>> = if stop.load(Ordering::Relaxed) {
        Some(Err("camera error -- see debug log".to_string()))
    } else {
        match mode.as_str() {
            "never_reports" => None,
            "fails" => Some(Err("Mock camera error".to_string())),
            _ => Some(
                path.as_deref()
                    .map_or(Ok(()), write_frame)
                    .map_err(|e| format!("error saving image file: {e}")),
            ),
        }
    };
    if matches!(outcome, Some(Ok(()))) {
        FRAME_TAKEN.store(true, Ordering::Relaxed);
    }
    *FRAME_IN_FLIGHT.lock().unwrap() = None;
    *app_state.lock().unwrap() = "Stopped".to_string();
    let Some(outcome) = outcome else {
        eprintln!("Mock PHD2: never_reports mode - withholding SingleFrameComplete");
        return;
    };
    // PHD2 leaves out `Error` on success and `Path` without a save.
    let mut event = serde_json::Map::new();
    event.insert("Event".to_string(), "SingleFrameComplete".into());
    event.insert("Success".to_string(), outcome.is_ok().into());
    if let Err(error) = outcome {
        event.insert("Error".to_string(), error.into());
    }
    if let Some(path) = path {
        event.insert("Path".to_string(), path.into());
    }
    broadcast(&serde_json::Value::Object(event).to_string());
    broadcast(r#"{"Event":"LoopingExposuresStopped"}"#);
}

/// Write the camera's frame at `path`, refusing a name that exists.
fn write_frame(path: &str) -> Result<(), String> {
    let (width, height) = frame_size();
    // `& 0xFFFF` is `mod 65536` for the unsigned index.
    let pixels: Vec<u16> = (0..width.saturating_mul(height))
        .map(|i| u16::try_from(i.wrapping_mul(SAVED_FRAME_STEP) & 0xFFFF).unwrap())
        .collect();
    let mut file =
        std::fs::File::create_new(path).map_err(|e| format!("cannot create {path}: {e}"))?;
    write_u16_image(&mut file, &pixels, width, height, &[])
        .map_err(|e| format!("cannot write {path}: {e}"))
}

/// An accepted connection, registered for events and ready to serve.
struct Client {
    reader: TcpStream,
    writer: Arc<Mutex<TcpStream>>,
    registered: Registered,
}

/// Make an accepted connection a client, on the accepting thread.
///
/// The socket is put back into blocking mode: Windows hands out an
/// accepted socket in its listener's non-blocking mode, and the client
/// loop relies on its read timeout rather than spinning on `WouldBlock`.
/// The client is sent its `Version` event, then registered, here, in
/// accept order, before its thread runs. So an event broadcast on behalf
/// of a later client (as `mock_report_frame` does) reaches every client
/// accepted before it, and only after its `Version`, as PHD2's event
/// server does. `None` when the socket cannot be set up or written to.
fn accept_client(stream: TcpStream) -> Option<Client> {
    let set_up = stream
        .set_nonblocking(false)
        .and_then(|()| stream.set_read_timeout(Some(std::time::Duration::from_secs(5))))
        .and_then(|()| stream.set_write_timeout(Some(std::time::Duration::from_secs(5))))
        .and_then(|()| stream.try_clone());
    match set_up {
        Ok(reader) => {
            let writer = Arc::new(Mutex::new(stream));
            if let Err(e) = write_line(&writer, &version_event()) {
                eprintln!("cannot send the Version event: {e}");
                return None;
            }
            eprintln!("Sent version event");
            let registered = Registered::new(&writer);
            Some(Client {
                reader,
                writer,
                registered,
            })
        }
        Err(e) => {
            eprintln!("cannot set up an accepted connection: {e}");
            None
        }
    }
}

/// The `Version` event PHD2 sends first on every connection.
fn version_event() -> String {
    let version = std::env::var("MOCK_PHD2_VERSION").unwrap_or_else(|_| "2.6.11-mock".to_string());
    serde_json::json!({
        "Event": "Version",
        "PHDVersion": version,
        "PHDSubver": "test",
        "MsgVersion": 1,
    })
    .to_string()
}

fn handle_client(
    client: Client,
    shutdown: &Arc<AtomicBool>,
    ignore_shutdown: bool,
    app_state: &AppState,
) {
    let Client {
        reader,
        writer,
        registered: _registered,
    } = client;
    let reader = BufReader::new(reader);

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

    // Mock responses: the arms that touch mock state or the
    // environment live here; everything else is a canned fixture.
    let result = match method {
        "get_app_state" => serde_json::json!(app_state.lock().unwrap().clone()),
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
            // A frame in flight ends early, reported as PHD2 reports an
            // interrupted exposure.
            if let Some(stop) = FRAME_IN_FLIGHT.lock().unwrap().as_ref() {
                stop.store(true, Ordering::Relaxed);
            }
            let stop_mode =
                std::env::var("MOCK_PHD2_STOP_MODE").unwrap_or_else(|_| "stops".to_string());
            if stop_mode == "never_stops" {
                eprintln!("Mock PHD2: never_stops mode - state unchanged");
            } else {
                *app_state.lock().unwrap() = "Stopped".to_string();
            }
            serde_json::json!(0)
        }
        "mock_report_frame" => report_frame(&params),
        "get_camera_frame_size" => {
            if camera_disconnected() {
                return format!(
                    r#"{{"jsonrpc":"2.0","error":{{"code":1,"message":"camera not connected"}},"id":{id}}}"#
                );
            }
            reported_frame_size()
        }
        "capture_single_frame" => match capture_single_frame(&params, app_state) {
            Ok(result) => result,
            Err(refusal) => return refusal.reply(&id),
        },
        "dither" => {
            emit_settle_sequence(writer.clone());
            serde_json::json!(0)
        }
        "save_image" => match save_image() {
            Ok(filename) => serde_json::json!({ "filename": filename }),
            Err(e) => {
                // PHD2's own reply when it cannot write the file.
                eprintln!("Mock PHD2: save_image failed: {e}");
                return format!(
                    r#"{{"jsonrpc":"2.0","error":{{"code":3,"message":"error saving image"}},"id":{id}}}"#
                );
            }
        },
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
        other => match canned_response(other) {
            Some(canned) => canned,
            None => {
                return format!(
                    r#"{{"jsonrpc":"2.0","error":{{"code":-32601,"message":"Method not found: {method}"}},"id":{id}}}"#
                );
            }
        },
    };

    format!(r#"{{"jsonrpc":"2.0","result":{result},"id":{id}}}"#)
}

/// Canned replies for the methods with no mock state to consult or
/// update: boolean getters report "off" / "not yet", setters and
/// one-shot commands acknowledge with the PHD2 success code 0, and the
/// profile / calibration / camera getters return fixed fixtures.
fn canned_response(method: &str) -> Option<serde_json::Value> {
    let value = match method {
        // Boolean getters the mock reports as "off" / "not yet".
        "get_connected" | "get_use_subframes" | "get_calibrated" | "get_paused" => {
            serde_json::json!(false)
        }
        // Setters and one-shot commands with no state to update here —
        // the mock acknowledges with the PHD2 success code 0.
        "set_connected" | "set_profile" | "set_exposure" | "clear_calibration"
        | "flip_calibration" | "set_lock_position" | "find_star" | "set_paused"
        | "set_algo_param" | "set_cooler_state" => {
            serde_json::json!(0)
        }
        "get_profiles" => serde_json::json!([
            {"id": 1, "name": "Mock Profile"}
        ]),
        "get_profile" => serde_json::json!({"id": 1, "name": "Mock Profile"}),
        "get_exposure" => serde_json::json!(1000),
        "get_exposure_durations" => serde_json::json!([100, 200, 500, 1000, 2000, 3000]),
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
        _ => return None,
    };
    Some(value)
}
