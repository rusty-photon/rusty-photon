//! The `doctor` subcommand: read-only diagnosis of this service's own config
//! through the same typed load path a start would use.
//!
//! The contract is docs/services/doctor.md §Per-service doctors': no server
//! starts, nothing is written, and the exit code is the shared one (0 = no
//! failures, 1 = at least one, 2 = the run itself broke).

use std::path::PathBuf;
use std::process::exit;

use crate::config::load_config;

pub fn run(config: Option<PathBuf>, json: bool) -> ! {
    let config_path = match rusty_photon_config::resolve_config_path("polar-align", config) {
        Ok(path) => path,
        Err(error) => {
            eprintln!("doctor: {error}");
            exit(2);
        }
    };
    let (output, code) = rusty_photon_doctor_checks::service::run(
        "polar-align",
        env!("CARGO_PKG_VERSION"),
        &config_path,
        |path| {
            load_config(path)
                .map(|_| ())
                .map_err(|error| error.to_string())
        },
        None,
        json,
    );
    print!("{output}");
    exit(code);
}
