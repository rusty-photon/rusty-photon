//! The `doctor` subcommand: read-only diagnosis of this service's own config
//! plus what the EAF SDK can see.
//!
//! The contract is docs/services/doctor.md §Per-service doctors': no server
//! starts, nothing is written, and the exit code is the shared one (0 = no
//! failures, 1 = at least one, 2 = the run itself broke).

use std::path::PathBuf;
use std::process::exit;

use rusty_photon_doctor_checks::service::SdkOutcome;

use crate::{load_effective_config, CliOverrides};

pub fn run(config: Option<PathBuf>, json: bool) -> ! {
    let config_path = match rusty_photon_config::resolve_config_path("zwo-focuser", config) {
        Ok(path) => path,
        Err(error) => {
            eprintln!("doctor: {error}");
            exit(2);
        }
    };
    let (output, code) = rusty_photon_doctor_checks::service::run(
        "zwo-focuser",
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

/// Enumeration only: `Sdk::focusers()` reads ids and properties without
/// opening a focuser, so doctor can run while the service holds the
/// device. The EAF is HID on Linux — access problems usually mean the
/// hidraw udev rule.
fn enumerate() -> SdkOutcome {
    let sdk = match zwo_rs::Sdk::new() {
        Ok(sdk) => sdk,
        Err(error) => return failure(&error),
    };
    match sdk.focusers() {
        Ok(infos) => SdkOutcome::Devices(infos.into_iter().map(|info| info.name).collect()),
        Err(error) => failure(&error),
    }
}

fn failure(error: &zwo_rs::Error) -> SdkOutcome {
    let suggestion = match error {
        // The error names what refused the SDK's log, and what to do.
        zwo_rs::Error::EafLog { .. } => None,
        _ => Some("check the USB connection and the installed ZWO udev rule".to_string()),
    };
    SdkOutcome::Error {
        detail: error.to_string(),
        suggestion,
    }
}

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
mod tests {
    use super::*;

    #[test]
    fn a_refused_sdk_log_is_reported_without_the_usb_hint() {
        let error = zwo_rs::Error::EafLog {
            dir: "/tmp/zwo/log/eaf_sdk".to_string(),
            reason: "/tmp/zwo (owner uid 0) refused the log".to_string(),
        };

        let SdkOutcome::Error { detail, suggestion } = failure(&error) else {
            panic!("expected SdkOutcome::Error");
        };
        assert_eq!(detail, error.to_string());
        assert_eq!(suggestion, None);
    }

    #[test]
    fn any_other_sdk_failure_points_at_the_usb_connection_and_udev() {
        let error = zwo_rs::Error::Eaf(zwo_rs::EafError::Removed);

        let SdkOutcome::Error { detail, suggestion } = failure(&error) else {
            panic!("expected SdkOutcome::Error");
        };
        assert_eq!(detail, error.to_string());
        assert_eq!(
            suggestion.as_deref(),
            Some("check the USB connection and the installed ZWO udev rule")
        );
    }
}
