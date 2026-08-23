//! The `doctor` subcommand: read-only diagnosis of this service's own config
//! plus what the ASI SDK can see.
//!
//! The contract is docs/services/doctor.md §Per-service doctors': no server
//! starts, nothing is written, and the exit code is the shared one (0 = no
//! failures, 1 = at least one, 2 = the run itself broke).

use std::path::PathBuf;
use std::process::exit;

use rusty_photon_doctor_checks::service::SdkOutcome;

use crate::{load_effective_config, CliOverrides};

pub fn run(config: Option<PathBuf>, json: bool) -> ! {
    let config_path = match rusty_photon_config::resolve_config_path("zwo-camera", config) {
        Ok(path) => path,
        Err(error) => {
            eprintln!("doctor: {error}");
            exit(2);
        }
    };
    let (output, code) = rusty_photon_doctor_checks::service::run(
        "zwo-camera",
        env!("CARGO_PKG_VERSION"),
        &config_path,
        |path| {
            load_effective_config(path, &CliOverrides::default())
                .map(|_| ())
                .map_err(|error| error.to_string())
        },
        Some(enumerate()),
        json,
    );
    print!("{output}");
    exit(code);
}

/// Enumeration only: `Sdk::cameras()` reads properties without opening a
/// camera. The service's own startup enumeration opens each camera
/// briefly for its serial — doctor must not, so it can run while the
/// service holds the device.
fn enumerate() -> SdkOutcome {
    let suggestion =
        || Some("check the USB connection and the installed ZWO udev rule".to_string());
    let sdk = match zwo_rs::Sdk::new() {
        Ok(sdk) => sdk,
        Err(error) => {
            return SdkOutcome::Error {
                detail: error.to_string(),
                suggestion: suggestion(),
            }
        }
    };
    match sdk.cameras() {
        Ok(infos) => SdkOutcome::Devices(infos.into_iter().map(|info| info.name).collect()),
        Err(error) => SdkOutcome::Error {
            detail: error.to_string(),
            suggestion: suggestion(),
        },
    }
}
