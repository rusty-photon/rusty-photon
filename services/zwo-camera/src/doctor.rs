//! The `doctor` subcommand: read-only diagnosis of this service's own config
//! plus what the ASI SDK can see.
//!
//! The contract is docs/services/doctor.md §Per-service doctors': no server
//! starts, nothing is written, and the exit code is the shared one (0 = no
//! failures, 1 = at least one, 2 = the run itself broke).

use std::path::{Path, PathBuf};
use std::process::exit;

use rusty_photon_doctor_checks::claims::{render_listing, Claims};
use rusty_photon_doctor_checks::service::SdkOutcome;

use crate::claims::{listing, sdk_cameras, UsbSource, NORMALIZER, SDK_NAME, SERVICE, VENDOR};
use crate::{load_effective_config, read_effective_config, CliOverrides, ZwoCameraError};

/// `doctor --devices`: the ASI cameras on the bus by USB port, and the
/// `usb_devices` block to paste (docs/services/zwo-camera.md U8).
///
/// Read-only and enumeration-only — no camera is opened, not even for its
/// serial — so it is safe to run while the service holds its cameras. Exits 0;
/// 1 when the config cannot be read, its list breaks a rule (the ports are
/// listed all the same — they are how the list gets fixed), the scan failed,
/// or the SDK could not enumerate; 2 when the config path cannot be resolved.
pub fn run_devices(config: Option<PathBuf>, source: &UsbSource) -> ! {
    let config_path = match rusty_photon_config::resolve_config_path(SERVICE, config) {
        Ok(path) => path,
        Err(error) => {
            eprintln!("doctor: {error}");
            exit(2);
        }
    };
    match devices(&config_path, source) {
        Ok((output, code)) => {
            print!("{output}");
            exit(code);
        }
        Err(error) => {
            eprintln!("doctor: {error}");
            exit(1);
        }
    }
}

/// The listing [`run_devices`] prints, and its exit code; `Err` when the
/// config cannot be read at all, which goes to stderr as the path error does.
fn devices(config_path: &Path, source: &UsbSource) -> Result<(String, i32), ZwoCameraError> {
    let config = read_effective_config(config_path, &CliOverrides::default())?;
    // Said whatever else fails, so a host fault fixed first does not hide it.
    let list_note = config.check_list().err().map(|error| {
        format!(
            "The usb_devices list in {} breaks a rule, so the service refuses to start: \
             {error}.",
            config_path.display()
        )
    });
    let with_list_note = |text: String| match &list_note {
        Some(note) => format!("{note}\n\n{text}"),
        None => text,
    };
    let scan = source.scan();
    if let Err(error) = &scan {
        return Ok((
            with_list_note(format!(
                "{SDK_NAME} cameras on the bus (vendor {VENDOR}):\n\n  the USB scan failed: \
                 {error}\n"
            )),
            1,
        ));
    }
    let infos = match sdk_camera_infos() {
        Ok(infos) => infos,
        Err(error) => {
            return Ok((
                with_list_note(format!("{SDK_NAME} SDK enumeration failed: {error}\n")),
                1,
            ))
        }
    };
    let claims = Claims::new(NORMALIZER, SERVICE, scan, sdk_cameras(&infos));
    let mut listing = listing(&config, &claims, &infos);
    let Some(note) = list_note else {
        return Ok((render_listing(&listing), 0));
    };
    // A block reproducing a list the service refuses would only be pasted
    // back; the ports listed are what the operator fixes it from.
    listing.block = None;
    listing
        .notes
        .insert(0, format!("{note} Fix it from the ports above."));
    Ok((render_listing(&listing), 1))
}

/// What the SDK enumerates, before any open: the one enumeration `doctor` and
/// `doctor --devices` read.
fn sdk_camera_infos() -> Result<Vec<zwo_rs::CameraInfo>, zwo_rs::Error> {
    zwo_rs::Sdk::new()?.cameras()
}

pub fn run(config: Option<PathBuf>, json: bool) -> ! {
    let config_path = match rusty_photon_config::resolve_config_path(SERVICE, config) {
        Ok(path) => path,
        Err(error) => {
            eprintln!("doctor: {error}");
            exit(2);
        }
    };
    let (output, code) = rusty_photon_doctor_checks::service::run(
        SERVICE,
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
/// camera. The service's own startup enumeration opens each camera it serves
/// briefly for its serial — doctor must not, so it can run while the service
/// holds the device.
fn enumerate() -> SdkOutcome {
    let suggestion =
        || Some("check the USB connection and the installed ZWO udev rule".to_string());
    match sdk_camera_infos() {
        Ok(infos) => SdkOutcome::Devices(infos.into_iter().map(|info| info.name).collect()),
        Err(error) => SdkOutcome::Error {
            detail: error.to_string(),
            suggestion: suggestion(),
        },
    }
}
