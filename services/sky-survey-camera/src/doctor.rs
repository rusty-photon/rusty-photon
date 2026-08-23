//! The `doctor` subcommand: read-only diagnosis of this service's own
//! config (docs/services/doctor.md §Per-service doctors).
//!
//! The diagnosis runs through the same typed load path a start would
//! use. No server starts, nothing is written, and the exit code follows
//! doctor's shared contract (0 = no failures, 1 = at least one, 2 = the
//! run itself broke).

use std::path::PathBuf;
use std::process::exit;

use crate::config::load_config;

pub fn run(config: Option<PathBuf>, json: bool) -> ! {
    let config_path = match rusty_photon_config::resolve_config_path("sky-survey-camera", config) {
        Ok(path) => path,
        Err(error) => {
            eprintln!("doctor: {error}");
            exit(2);
        }
    };
    let (output, code) = rusty_photon_doctor_checks::service::run(
        "sky-survey-camera",
        env!("CARGO_PKG_VERSION"),
        &config_path,
        |path| {
            // `load_config` is async (tokio::fs) because startup runs it
            // inside the service runtime; doctor dispatches before any
            // runtime exists, so give the load its own minimal one. Still
            // strictly read-only — the load reads, parses, and validates,
            // never writes.
            tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .map_err(|error| error.to_string())?
                .block_on(load_config(path))
                .map(|_| ())
                .map_err(|error| error.to_string())
        },
        None,
        json,
    );
    print!("{output}");
    exit(code);
}
