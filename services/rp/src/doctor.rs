//! The `doctor` subcommand (docs/services/doctor.md §Per-service doctors):
//! read-only diagnosis of this service's own config through the same typed
//! load path a start would use.
//!
//! No server starts, nothing is written, and
//! the exit code follows doctor's shared contract (0 = no failures, 1 =
//! at least one, 2 = the run itself broke).
//!
//! Unlike the serve path (`resolve_and_init`, which materializes the
//! first-start scaffold), this resolves via `resolve_config_path` — an
//! absent file must stay absent.

use std::path::PathBuf;
use std::process::exit;

pub fn run(config: Option<PathBuf>, json: bool) -> ! {
    let config_path = match rusty_photon_config::resolve_config_path("rp", config) {
        Ok(path) => path,
        Err(error) => {
            eprintln!("doctor: {error}");
            exit(2);
        }
    };
    let (output, code) = rusty_photon_doctor_checks::service::run(
        "rp",
        env!("CARGO_PKG_VERSION"),
        &config_path,
        |path| {
            crate::config::load_config(path)
                .map(|_| ())
                .map_err(|error| error.to_string())
        },
        None,
        json,
    );
    print!("{output}");
    exit(code);
}
