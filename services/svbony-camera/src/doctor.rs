//! The `doctor` subcommand: read-only diagnosis of this service's own config
//! plus what the `SVBony` SDK can see.
//!
//! The contract is docs/services/doctor.md §Per-service doctors': no server
//! starts, nothing is written, and the exit code is the shared one (0 = no
//! failures, 1 = at least one, 2 = the run itself broke).

use std::path::{Path, PathBuf};
use std::process::exit;

use rusty_photon_doctor_checks::claims::{
    devices_report, usb_devices_checks, BlockEntry, ListEntry,
};
use rusty_photon_doctor_checks::report::Check;
use rusty_photon_doctor_checks::service::{self, SdkOutcome};

use crate::claims::{listing, sdk_cameras, UsbSource, NORMALIZER, SERVICE};
use crate::config::Config;
use crate::{
    load_effective_config, read_effective_config, sdk_camera_infos, CliOverrides, SvbonyCameraError,
};

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
fn devices(config_path: &Path, source: &UsbSource) -> Result<(String, i32), SvbonyCameraError> {
    let config = read_effective_config(config_path, &CliOverrides::default())?;
    let list = configured_list(&config);
    Ok(devices_report(
        NORMALIZER,
        SERVICE,
        config_path,
        config.check_list().map(|()| list.as_deref()),
        source.scan(),
        || {
            let infos = sdk_camera_infos().map_err(|error| error.to_string())?;
            Ok((sdk_cameras(&infos), infos))
        },
        |claims, infos| listing(&config, claims, infos),
    ))
}

/// The `usb_devices` entries `config` lists, as the block reproduces them;
/// `None` without a list.
fn configured_list(config: &Config) -> Option<Vec<BlockEntry>> {
    config
        .usb_devices
        .as_ref()
        .map(|list| list.iter().map(ListEntry::block_entry).collect())
}

/// The report: `config.full-shape`, `hardware.sdk-devices`, and the
/// `usb-devices.*` checks of the claims a start would make from `source`'s
/// scan (U11).
pub fn run(config: Option<PathBuf>, json: bool, source: &UsbSource) -> ! {
    let config_path = match rusty_photon_config::resolve_config_path(SERVICE, config) {
        Ok(path) => path,
        Err(error) => {
            eprintln!("doctor: {error}");
            exit(2);
        }
    };
    let mut checks = vec![
        service::full_shape_check(&config_path, |path| {
            load_effective_config(path, &CliOverrides::default())
                .map(|_| ())
                .map_err(|error| error.to_string())
        }),
        service::sdk_devices_check(enumerate()),
    ];
    checks.extend(claims_checks(&config_path, source));
    let (output, code) = service::emit(
        SERVICE,
        env!("CARGO_PKG_VERSION"),
        &config_path,
        checks,
        json,
    );
    print!("{output}");
    exit(code);
}

/// The `usb-devices.*` checks (U11); none when the config does not load,
/// which `config.full-shape` reports.
fn claims_checks(config_path: &Path, source: &UsbSource) -> Vec<Check> {
    let Ok(config) = load_effective_config(config_path, &CliOverrides::default()) else {
        return Vec::new();
    };
    let list = configured_list(&config);
    usb_devices_checks(
        NORMALIZER,
        SERVICE,
        list.as_deref(),
        source.scan(),
        || {
            let infos = sdk_camera_infos().map_err(|error| error.to_string())?;
            Ok((sdk_cameras(&infos), infos))
        },
        |claims, infos| listing(&config, claims, infos),
    )
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
