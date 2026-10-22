//! svbony-camera's half of the device claims (docs/services/svbony-camera.md
//! U1-U11).
//!
//! Which SDK models it knows on the bus (the normalizer, U2), its simulated
//! USB scan (U7), what `doctor --devices` can say about each camera without
//! opening it (U8), and the placeholder it registers (U4). The join, the list's
//! validation, port order, the scan source and the listing are
//! `rusty-photon-doctor-checks`' `claims` module, and the placeholder device
//! is `rusty-photon-driver`'s, shared with the other camera drivers.

use std::collections::BTreeMap;
use std::sync::Arc;

pub use rusty_photon_doctor_checks::claims::UsbSource;
use rusty_photon_doctor_checks::claims::{
    self, CameraFacts, Claims, DisplayNames, Listing, Normalizer, SdkCamera,
};
use rusty_photon_doctor_checks::UsbScan;
use rusty_photon_driver::claims::{BusCamera, Placeholder, PlaceholderCamera, ScanCameras};
use svbony_rs::CameraInfo;

use crate::config::{Config, UsbDeviceEntry};
use crate::setup::SvbonyCameraDriver;

/// The service name: the config file's stem and the placeholder `UniqueID`'s
/// middle field.
pub const SERVICE: &str = "svbony-camera";

/// The SDK's display name, for messages.
pub const SDK_NAME: &str = "SVBony";

/// `SVBony`'s USB vendor id.
pub const VENDOR: &str = "f266";

/// The model the `svbony-rs` simulation fabricates.
#[cfg(feature = "simulation")]
pub const SIMULATED_MODEL: &str = "SV605CC-Simulated";

/// The SV605CC as each side names it, both observed on the same unit: the
/// SDK's `friendly_name` is `SVBONY SV605CC` (its `UniqueID`,
/// `SVBONY:SVBONY-SV605CC:…`, in every validation record since 2026-07-26),
/// and the bus shows `f266:9a0a`, product string `SVBONY SV605CC`, no USB
/// serial (pier1, Raspberry Pi 5, Linux, 2026-10-10).
const SV605CC: (&str, &str) = ("SVBONY SV605CC", "9a0a");

/// Each SDK model this driver has seen on the bus, by the name the SDK gives
/// it, with the product id it enumerates under — observed pairs only, since a
/// guess here serves the wrong camera at the wrong number.
#[cfg(not(feature = "simulation"))]
const MODELS: &[(&str, &str)] = &[SV605CC];

/// The observed models, plus the simulation's fabricated mirror of the
/// SV605CC under the same product id (U7).
#[cfg(feature = "simulation")]
const MODELS: &[(&str, &str)] = &[SV605CC, (SIMULATED_MODEL, "9a0a")];

/// What this driver knows about `SVBony` cameras on the bus. The `SVBony` SDK
/// has no passive check of a product id.
pub const NORMALIZER: Normalizer<'static> = Normalizer {
    sdk: SDK_NAME,
    vendor: VENDOR,
    models: MODELS,
    not_cameras: &[],
    sdk_check: None,
};

/// The SDK's cameras as the join reads them.
///
/// `CameraSN` is not offered as a serial: no `SVBony` camera has been seen
/// publishing a USB serial, so how the bus would spell one beside `CameraSN` is
/// unknown — and a serial both sides carry in different spellings would refuse
/// the right camera (U2).
#[must_use]
pub fn sdk_cameras<'a>(infos: impl IntoIterator<Item = &'a CameraInfo>) -> Vec<SdkCamera> {
    infos
        .into_iter()
        .map(|info| SdkCamera {
            model: info.friendly_name.clone(),
            serial: None,
        })
        .collect()
}

/// Where this driver's scan comes from when nothing chose otherwise: the
/// host, the only source a release build has.
#[cfg(not(feature = "simulation"))]
#[must_use]
pub const fn default_usb_source() -> UsbSource {
    UsbSource::Host
}

/// Where this driver's scan comes from when nothing chose otherwise: the
/// fabricated cameras' records, since a simulation build never scans the host
/// (U7).
#[cfg(feature = "simulation")]
#[must_use]
pub fn default_usb_source() -> UsbSource {
    UsbSource::Simulated(simulated_inventory())
}

/// The source a simulation build's hidden `--usb-inventory` flag selects
/// (U7): the staged document in `path`, or without it the simulated cameras'
/// records.
///
/// # Errors
///
/// Why the staged document is unusable, naming the file.
#[cfg(feature = "simulation")]
pub fn usb_source_from_flag(path: Option<std::path::PathBuf>) -> Result<UsbSource, String> {
    UsbSource::from_flag(path, simulated_inventory())
}

/// One working record per camera the `svbony-rs` simulation fabricates:
/// `f266:9a0a` on port `simulated-usbv3-0:<n>` for the camera at SDK index
/// `n - 1`.
#[cfg(feature = "simulation")]
fn simulated_inventory() -> rusty_photon_doctor_checks::UsbScan {
    rusty_photon_doctor_checks::UsbScan {
        devices: (1..=svbony_rs::SIM_CAMERA_COUNT)
            .map(|n| rusty_photon_doctor_checks::UsbDevice {
                vendor: VENDOR.to_string(),
                product: "9a0a".to_string(),
                model: Some(format!("SVBONY {SIMULATED_MODEL}")),
                port: Some(format!("simulated-usbv3-0:{n}")),
                serial: None,
            })
            .collect(),
        faults: Vec::new(),
    }
}

/// The rows the setup pages' Cameras section shows from `scan`: every
/// `f266` record the normalizer can count as a camera, named by its product
/// string, else `f266:<pid>` (*Setup pages*).
#[must_use]
pub fn bus_cameras(scan: &UsbScan) -> Vec<BusCamera> {
    NORMALIZER
        .camera_records(scan)
        .into_iter()
        .filter_map(|record| {
            Some(BusCamera {
                port: record.port.clone()?,
                label: record
                    .model
                    .clone()
                    .unwrap_or_else(|| format!("{}:{}", record.vendor, record.product)),
            })
        })
        .collect()
}

/// The scan the Cameras section runs for each server page: `source`'s, the
/// one a start takes (U1), so nothing is opened.
#[must_use]
pub fn scan_cameras(source: UsbSource) -> ScanCameras {
    Arc::new(move || source.scan().map(|scan| bus_cameras(&scan)))
}

/// The placeholder that holds `entry`'s number for `reason` (U4).
#[must_use]
pub fn placeholder(
    entry: &UsbDeviceEntry,
    reason: String,
) -> PlaceholderCamera<SvbonyCameraDriver> {
    PlaceholderCamera::new(Placeholder {
        name: entry
            .name
            .clone()
            .unwrap_or_else(|| claims::placeholder_name(SDK_NAME, &entry.usb_port)),
        unique_id: claims::placeholder_unique_id(SERVICE, &entry.usb_port),
        usb_port: entry.usb_port.clone(),
        reason,
        driver_info: crate::camera::DRIVER_INFO.to_string(),
        driver_version: env!("CARGO_PKG_VERSION").to_string(),
        driver_code: claims::PLACEHOLDER_DRIVER_CODE,
    })
}

/// The key a camera's `devices` override is read under, and the last field of
/// its `UniqueID`: its `CameraSN`, or `noserial-<port>` for a camera that
/// reports none, `port` being the one the join placed it on.
#[must_use]
pub fn override_key(info: &CameraInfo, port: &str) -> String {
    if info.serial.is_empty() {
        format!("noserial-{port}")
    } else {
        info.serial.clone()
    }
}

/// What `doctor --devices` prints for `config`, the join in `claims` and the
/// SDK's cameras (U8).
///
/// `CameraSN` is free at enumeration, so it is the SDK id and, with
/// `noserial-<port>` for a camera that reports none, the key each placed
/// camera's `devices` override is matched by.
#[must_use]
pub fn listing(config: &Config, claims: &Claims<'_>, infos: &[CameraInfo]) -> Listing {
    let ports: BTreeMap<usize, &str> = claims.placed().into_iter().collect();
    let cameras: Vec<CameraFacts> = infos
        .iter()
        .enumerate()
        .map(|(index, info)| CameraFacts {
            model: info.friendly_name.clone(),
            sdk_id: (!info.serial.is_empty()).then(|| info.serial.clone()),
            override_key: ports.get(&index).map(|port| override_key(info, port)),
        })
        .collect();
    let list = config.usb_devices.as_deref().map(claims::block_entries);
    let overrides: BTreeMap<String, DisplayNames> = config
        .devices
        .iter()
        .map(|(key, o)| {
            (
                key.clone(),
                DisplayNames {
                    name: o.name.clone(),
                    description: o.description.clone(),
                    ..DisplayNames::default()
                },
            )
        })
        .collect();
    claims.listing(&cameras, list.as_deref(), &overrides)
}

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
mod tests {
    use super::*;
    use crate::config::DeviceOverride;
    use ascom_alpaca::api::Device;
    use rusty_photon_doctor_checks::{UsbDevice, UsbScan};

    fn info(name: &str, serial: &str) -> CameraInfo {
        CameraInfo {
            id: 0,
            friendly_name: name.to_string(),
            serial: serial.to_string(),
            port_type: "USB3".to_string(),
            device_id: 0,
        }
    }

    fn sv605cc(port: &str) -> UsbDevice {
        UsbDevice {
            vendor: "f266".to_string(),
            product: "9a0a".to_string(),
            model: Some("SVBONY SV605CC".to_string()),
            port: Some(port.to_string()),
            serial: None,
        }
    }

    #[test]
    fn the_cameras_section_rows_are_the_svbony_records_named_as_the_bus_names_them() {
        let other_vendor = UsbDevice {
            vendor: "03c3".to_string(),
            ..sv605cc("p:3")
        };
        let unnamed = UsbDevice {
            model: None,
            ..sv605cc("p:2")
        };
        let scan = UsbScan {
            devices: vec![sv605cc("p:1"), unnamed, other_vendor],
            faults: Vec::new(),
        };
        assert_eq!(
            bus_cameras(&scan),
            vec![
                BusCamera {
                    port: "p:1".to_string(),
                    label: "SVBONY SV605CC".to_string(),
                },
                BusCamera {
                    port: "p:2".to_string(),
                    label: "f266:9a0a".to_string(),
                },
            ]
        );
    }

    fn claims_for(devices: Vec<UsbDevice>, infos: &[CameraInfo]) -> Claims<'static> {
        Claims::new(
            NORMALIZER,
            SERVICE,
            Ok(UsbScan {
                devices,
                faults: Vec::new(),
            }),
            sdk_cameras(infos),
        )
    }

    /// The names as the hardware gives them, copied from the records rather
    /// than from the table: the SDK's `friendly_name` from the 2026-07-26
    /// validation's `UniqueID` (`SVBONY:SVBONY-SV605CC:0123481353808C03EE2512150035`),
    /// the bus record from pier1's sysfs.
    #[test]
    fn the_sv605cc_is_placed_by_the_name_the_sdk_gives_it() {
        let claims = claims_for(
            vec![sv605cc("platform-xhci-hcd.1-usbv3-0:1")],
            &[info("SVBONY SV605CC", "0123481353808C03EE2512150035")],
        );
        assert_eq!(claims.placed(), vec![(0, "platform-xhci-hcd.1-usbv3-0:1")]);
    }

    #[test]
    fn camera_sn_is_not_offered_to_the_join() {
        assert_eq!(sdk_cameras(&[info("SV605CC", "SN")])[0].serial, None);
    }

    #[test]
    fn a_camera_without_a_serial_is_keyed_by_its_port() {
        assert_eq!(
            override_key(&info("SV605CC", ""), "simulated-usbv3-0:2"),
            "noserial-simulated-usbv3-0:2"
        );
        assert_eq!(
            override_key(&info("SV605CC", "SN"), "simulated-usbv3-0:2"),
            "SN"
        );
    }

    #[test]
    fn with_no_list_the_block_carries_the_override_names_in_port_order() {
        let mut config = Config::default();
        config.devices.insert(
            "B".to_string(),
            DeviceOverride {
                name: Some("Main".to_string()),
                description: None,
            },
        );
        config
            .devices
            .insert("stale".to_string(), DeviceOverride::default());
        let infos = [info("SV605CC", "A"), info("SV605CC-X", "B")];
        // Two models, so each joins its own record; port order puts B first.
        let mut b = sv605cc("1-4.2");
        b.product = "9a0b".to_string();
        let claims = Claims::new(
            Normalizer {
                models: &[("SV605CC", "9a0a"), ("SV605CC-X", "9a0b")],
                ..NORMALIZER
            },
            SERVICE,
            Ok(UsbScan {
                devices: vec![sv605cc("1-4.10"), b],
                faults: Vec::new(),
            }),
            sdk_cameras(&infos),
        );
        let listing = listing(&config, &claims, &infos);
        let block = listing.block.unwrap();
        assert_eq!(block[0].usb_port, "1-4.2");
        assert_eq!(block[0].name.as_deref(), Some("Main"));
        assert_eq!(block[1].usb_port, "1-4.10");
        assert_eq!(block[1].name, None);
        assert_eq!(
            listing.rows[0].device, "0",
            "without a list it is served at its place in port order"
        );
        assert!(
            !listing.notes.iter().any(|n| n.contains("renumber")),
            "the block pins the numbers served now: {:?}",
            listing.notes
        );
        assert!(
            listing
                .paste_notes
                .iter()
                .any(|n| n.contains("devices override stale")),
            "{:?}",
            listing.paste_notes
        );
        assert!(
            listing
                .paste_notes
                .iter()
                .any(|n| n.starts_with("Delete the devices map from svbony-camera.json")),
            "{:?}",
            listing.paste_notes
        );
    }

    #[test]
    fn with_a_list_the_block_reproduces_it_and_marks_an_unlisted_camera() {
        let config = Config {
            usb_devices: Some(vec![
                UsbDeviceEntry {
                    device_number: 1,
                    usb_port: "p9".to_string(),
                    name: None,
                    description: None,
                },
                UsbDeviceEntry {
                    device_number: 0,
                    usb_port: "p8".to_string(),
                    name: Some("Guide port".to_string()),
                    description: None,
                },
            ]),
            ..Config::default()
        };
        let infos = [info("SVBONY SV605CC", "SN")];
        let claims = claims_for(vec![sv605cc("p1")], &infos);
        let listing = listing(&config, &claims, &infos);
        assert_eq!(listing.rows[0].device, "not listed");
        let block = listing.block.unwrap();
        assert_eq!(
            block
                .iter()
                .map(|e| e.usb_port.as_str())
                .collect::<Vec<_>>(),
            vec!["p8", "p9"]
        );
    }

    #[test]
    fn with_no_list_and_no_placed_camera_there_is_nothing_to_paste() {
        let claims = claims_for(Vec::new(), &[]);
        assert_eq!(listing(&Config::default(), &claims, &[]).block, None);
    }

    #[cfg(feature = "simulation")]
    #[test]
    fn the_simulated_inventory_places_the_simulated_camera() {
        let sdk = svbony_rs::Sdk::new().unwrap();
        let infos = sdk.cameras().unwrap();
        assert_eq!(infos[0].friendly_name, SIMULATED_MODEL);
        let claims = Claims::new(
            NORMALIZER,
            SERVICE,
            default_usb_source().scan(),
            sdk_cameras(&infos),
        );
        assert_eq!(claims.placed(), vec![(0, "simulated-usbv3-0:1")]);
    }

    fn entry(name: Option<&str>) -> UsbDeviceEntry {
        UsbDeviceEntry {
            device_number: 0,
            usb_port: "pci-0000:00:14.0-usbv3-0:4.2".to_string(),
            name: name.map(str::to_string),
            description: None,
        }
    }

    #[test]
    fn a_placeholder_answers_to_the_placeholder_unique_id_of_its_port() {
        assert_eq!(
            placeholder(&entry(None), String::new()).unique_id(),
            "placeholder:svbony-camera:pci-0000:00:14.0-usbv3-0:4.2"
        );
    }

    #[test]
    fn a_placeholders_name_names_the_port_unless_the_entry_names_it() {
        assert_eq!(
            placeholder(&entry(None), String::new()).static_name(),
            "SVBony camera on pci-0000:00:14.0-usbv3-0:4.2 (placeholder)"
        );
        assert_eq!(
            placeholder(&entry(Some("Main")), String::new()).static_name(),
            "Main"
        );
    }

    #[tokio::test]
    async fn a_placeholder_refuses_a_connect_with_the_device_claims_code() {
        let error = placeholder(&entry(None), "the USB scan failed".to_string())
            .set_connected(true)
            .await
            .unwrap_err();
        assert_eq!(error.code.raw(), claims::PLACEHOLDER_ERROR_CODE);
        assert!(error.message.ends_with("the USB scan failed"), "{error}");
    }

    #[tokio::test]
    async fn a_placeholder_answers_this_drivers_driver_info() {
        assert_eq!(
            placeholder(&entry(None), String::new())
                .driver_info()
                .await
                .unwrap(),
            crate::camera::DRIVER_INFO
        );
    }
}
