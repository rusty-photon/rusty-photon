//! The `doctor` subcommand: read-only diagnosis of this service's own config
//! plus what the `SVBony` SDK can see.
//!
//! The contract is docs/services/doctor.md §Per-service doctors': no server
//! starts, nothing is written, and the exit code is the shared one (0 = no
//! failures, 1 = at least one, 2 = the run itself broke).

use std::path::{Path, PathBuf};
use std::process::exit;

use rusty_photon_doctor_checks::claims::{render_listing, Claims};
use rusty_photon_doctor_checks::service::SdkOutcome;

use crate::claims::{listing, sdk_cameras, UsbSource, NORMALIZER, SDK_NAME, SERVICE, VENDOR};
use crate::{load_effective_config, read_effective_config, sdk_camera_infos, CliOverrides};

/// `doctor --devices`: the `SVBony` cameras on the bus by USB port, and the
/// `usb_devices` block to paste (docs/services/svbony-camera.md U8).
///
/// Read-only and enumeration-only — nothing is opened — so it is safe to run
/// while the service holds its cameras. Exits 0; 1 when the config cannot be
/// read, its list breaks a rule (the ports are listed all the same — they are
/// how the list gets fixed), the scan failed, or the SDK could not enumerate;
/// 2 when the config path cannot be resolved.
pub fn run_devices(config: Option<PathBuf>, source: &UsbSource) -> ! {
    let config_path = match rusty_photon_config::resolve_config_path(SERVICE, config) {
        Ok(path) => path,
        Err(error) => {
            eprintln!("doctor: {error}");
            exit(2);
        }
    };
    let (output, code) = devices(&config_path, source);
    print!("{output}");
    exit(code);
}

/// The listing [`run_devices`] prints, and its exit code.
fn devices(config_path: &Path, source: &UsbSource) -> (String, i32) {
    let config = match read_effective_config(config_path, &CliOverrides::default()) {
        Ok(config) => config,
        Err(error) => return (format!("{error}\n"), 1),
    };
    let list_error = config.check_list().err();
    let scan = source.scan();
    if let Err(error) = &scan {
        return (
            format!(
                "{SDK_NAME} cameras on the bus (vendor {VENDOR}):\n\n  the USB scan failed: \
                 {error}\n"
            ),
            1,
        );
    }
    let infos = match sdk_camera_infos() {
        Ok(infos) => infos,
        Err(error) => return (format!("{SDK_NAME} SDK enumeration failed: {error}\n"), 1),
    };
    let claims = Claims::new(NORMALIZER, SERVICE, scan, sdk_cameras(&infos));
    let mut listing = listing(&config, &claims, &infos);
    let Some(error) = list_error else {
        return (render_listing(&listing), 0);
    };
    // A block reproducing a list the service refuses would only be pasted
    // back; the ports above are what the operator fixes it from.
    listing.block = None;
    listing.notes.insert(
        0,
        format!(
            "The usb_devices list in {} breaks a rule, so the service refuses to start: \
             {error}. Fix it from the ports above.",
            config_path.display()
        ),
    );
    (render_listing(&listing), 1)
}

pub fn run(config: Option<PathBuf>, json: bool) -> ! {
    let config_path = match rusty_photon_config::resolve_config_path("svbony-camera", config) {
        Ok(path) => path,
        Err(error) => {
            eprintln!("doctor: {error}");
            exit(2);
        }
    };
    let (output, code) = rusty_photon_doctor_checks::service::run(
        "svbony-camera",
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
/// camera — `SVBony`'s `CameraSN` arrives at enumeration time (unlike ZWO), so
/// there is no need to open (and thus contend with the running service for)
/// any device just to list what is attached.
fn enumerate() -> SdkOutcome {
    let suggestion =
        || Some("check the USB connection and the installed SVBony udev rule".to_string());
    match sdk_camera_infos() {
        Ok(infos) => {
            SdkOutcome::Devices(infos.into_iter().map(|info| info.friendly_name).collect())
        }
        Err(error) => SdkOutcome::Error {
            detail: error.to_string(),
            suggestion: suggestion(),
        },
    }
}
