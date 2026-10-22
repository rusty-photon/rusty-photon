//! zwo-camera's half of the device claims (docs/services/zwo-camera.md
//! U1-U9).
//!
//! Which ASI models it knows on the bus and the ASI SDK's own check of a
//! product id (the normalizer, U2), its simulated USB scan (U7), what
//! `doctor --devices` can say about each camera without opening it (U8), and
//! the placeholder it registers (U4). The join, the list's validation, port
//! order, the scan source and the listing are `rusty-photon-doctor-checks`'
//! `claims` module, and the placeholder device is `rusty-photon-driver`'s,
//! shared with the other camera drivers.

use std::collections::BTreeMap;
use std::sync::Arc;

pub use rusty_photon_doctor_checks::claims::UsbSource;
use rusty_photon_doctor_checks::claims::{
    self, CameraFacts, Claims, DisplayNames, Listing, Normalizer, SdkCamera,
};
use rusty_photon_doctor_checks::UsbScan;
use rusty_photon_driver::claims::{BusCamera, Placeholder, PlaceholderCamera, ScanCameras};
use zwo_rs::CameraInfo;

use crate::config::{Config, UsbDeviceEntry};
use crate::setup::ZwoCameraDriver;

/// The service name: the config file's stem and the placeholder `UniqueID`'s
/// middle field.
pub const SERVICE: &str = "zwo-camera";

/// The SDK's display name, for messages.
pub const SDK_NAME: &str = "ZWO";

/// ZWO's USB vendor id, as the inventory spells it. The EAF focuser and the
/// EFW filter wheel enumerate under it too.
pub const VENDOR: &str = "03c3";

/// The model the `zwo-rs` simulation fabricates.
#[cfg(feature = "simulation")]
pub const SIMULATED_MODEL: &str = "ASI2600MM-Pro-Simulated";

/// [`zwo_rs::SIM_CAMERA_PRODUCT_ID`] as the inventory spells it: the product
/// id the simulated camera is placed by and staged under.
#[cfg(feature = "simulation")]
const SIMULATED_PRODUCT: &str = "2600";

/// Each ASI model this driver has seen on the bus, by the name the SDK gives
/// it, with the product id it enumerates under — observed pairs only, since a
/// guess here serves the wrong camera at the wrong number. All three were read
/// on the dev box (Fedora 44, `x86_64`, ASI SDK 1.41) on 2026-10-11: the SDK
/// names from `zwo-camera doctor`'s `hardware.sdk-devices` (and every
/// validation record's `UniqueID`, `ZWO:ZWO-ASI178MM:…`), the bus records from
/// sysfs, none of them publishing a USB serial.
const OBSERVED: [(&str, &str); 3] = [
    ("ZWO ASI1600MM-Cool", "1603"),
    ("ZWO ASI178MM", "178c"),
    ("ZWO ASI120MC-S", "120e"),
];

#[cfg(not(feature = "simulation"))]
const MODELS: &[(&str, &str)] = &OBSERVED;

/// The observed models, plus the simulation's fabricated camera under the
/// product id the simulation gives it (U7).
#[cfg(feature = "simulation")]
const MODELS: &[(&str, &str)] = &[
    OBSERVED[0],
    OBSERVED[1],
    OBSERVED[2],
    (SIMULATED_MODEL, SIMULATED_PRODUCT),
];

/// What this driver knows about ASI cameras on the bus. A product id it has
/// not observed is a camera's when the ASI SDK's own table says so, which is
/// what keeps the EAF and the EFW out (U2).
pub const NORMALIZER: Normalizer<'static> = Normalizer {
    sdk: SDK_NAME,
    vendor: VENDOR,
    models: MODELS,
    not_cameras: &[],
    sdk_check: Some(sdk_supports),
};

/// Whether the ASI SDK supports one of ZWO's product ids, as the inventory
/// spells it, as a camera (`ASICameraCheck`, which opens nothing).
fn sdk_supports(product: &str) -> bool {
    u16::from_str_radix(product, 16)
        .is_ok_and(|product| zwo_rs::is_camera_product(zwo_rs::ZWO_VENDOR_ID, product))
}

/// The SDK's cameras as the join reads them: their names alone. An ASI
/// camera's serial is readable only once it is open, which placing it must not
/// do (U2).
#[must_use]
pub fn sdk_cameras<'a>(infos: impl IntoIterator<Item = &'a CameraInfo>) -> Vec<SdkCamera> {
    infos
        .into_iter()
        .map(|info| SdkCamera {
            model: info.name.clone(),
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

/// One working record per camera the `zwo-rs` simulation fabricates:
/// `03c3:2600` on port `simulated-usbv3-0:<n>` for the camera at SDK index
/// `n - 1`.
#[cfg(feature = "simulation")]
fn simulated_inventory() -> rusty_photon_doctor_checks::UsbScan {
    rusty_photon_doctor_checks::UsbScan {
        devices: (1..=zwo_rs::SIM_CAMERA_COUNT)
            .map(|n| rusty_photon_doctor_checks::UsbDevice {
                vendor: VENDOR.to_string(),
                product: SIMULATED_PRODUCT.to_string(),
                model: Some(SIMULATED_MODEL.to_string()),
                port: Some(format!("simulated-usbv3-0:{n}")),
                serial: None,
            })
            .collect(),
        faults: Vec::new(),
    }
}

/// The rows the setup pages' Cameras section shows from `scan`: every
/// `03c3` record the normalizer can count as a camera, named by its product
/// string, else `03c3:<pid>` (*Setup pages*).
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
pub fn placeholder(entry: &UsbDeviceEntry, reason: String) -> PlaceholderCamera<ZwoCameraDriver> {
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

/// What `doctor --devices` prints for `config`, the join in `claims` and the
/// SDK's cameras (U8).
///
/// Without an open there is no SDK id to show, and no telling which camera a
/// `devices` override names: its key is the camera's serial, or
/// `noserial-<port>` when it has none, and telling which needs an open.
#[must_use]
pub fn listing(config: &Config, claims: &Claims<'_>, infos: &[CameraInfo]) -> Listing {
    let cameras: Vec<CameraFacts> = infos
        .iter()
        .map(|info| CameraFacts {
            model: info.name.clone(),
            sdk_id: None,
            override_key: None,
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

    /// A camera as the SDK lists it, by name; the rest is not read by the
    /// join.
    fn info(name: &str) -> CameraInfo {
        CameraInfo {
            id: 0,
            name: name.to_string(),
            max_width: 1,
            max_height: 1,
            is_color: false,
            bayer_pattern: zwo_rs::BayerPattern::Rg,
            supported_bins: vec![1],
            supported_video_formats: Vec::new(),
            pixel_size_um: 1.0,
            has_mechanical_shutter: false,
            has_st4_port: false,
            is_cooler_cam: false,
            is_usb3: true,
            e_per_adu: 1.0,
            bit_depth: 16,
            is_trigger_cam: false,
        }
    }

    /// A record as sysfs showed it on the dev box: product string without the
    /// SDK's `ZWO ` prefix, no USB serial.
    fn record(product: &str, model: &str, port: &str) -> UsbDevice {
        UsbDevice {
            vendor: "03c3".to_string(),
            product: product.to_string(),
            model: Some(model.to_string()),
            port: Some(port.to_string()),
            serial: None,
        }
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

    /// The dev box as it was read on 2026-10-11: the SDK lists the three
    /// cameras in this order, and each sits on its own port.
    #[test]
    fn the_observed_cameras_are_placed_by_the_names_the_sdk_gives_them() {
        let claims = claims_for(
            vec![
                record("178c", "ASI178MM", "pci-0000:c1:00.4-usbv2-0:1.2.1"),
                record("120e", "ASI120MC-S", "pci-0000:c1:00.4-usbv2-0:1.2.2"),
                record("1603", "ASI1600MM-Cool", "pci-0000:c1:00.4-usbv3-0:1.4"),
            ],
            &[
                info("ZWO ASI1600MM-Cool"),
                info("ZWO ASI120MC-S"),
                info("ZWO ASI178MM"),
            ],
        );
        assert_eq!(
            claims.placed(),
            vec![
                (2, "pci-0000:c1:00.4-usbv2-0:1.2.1"),
                (1, "pci-0000:c1:00.4-usbv2-0:1.2.2"),
                (0, "pci-0000:c1:00.4-usbv3-0:1.4"),
            ]
        );
    }

    #[test]
    fn no_serial_is_offered_to_the_join() {
        assert_eq!(sdk_cameras(&[info("ZWO ASI178MM")])[0].serial, None);
    }

    #[test]
    fn two_cameras_of_one_model_are_look_alikes() {
        let claims = claims_for(
            vec![
                record("178c", "ASI178MM", "p1"),
                record("178c", "ASI178MM", "p2"),
            ],
            &[info("ZWO ASI178MM"), info("ZWO ASI178MM")],
        );
        assert_eq!(claims.placed(), Vec::<(usize, &str)>::new());
    }

    #[cfg(feature = "simulation")]
    #[test]
    fn a_focuser_is_no_camera_record_and_leaves_the_camera_its_own() {
        let claims = claims_for(
            vec![
                record("1f10", "ZWO EAF", "p1"),
                record("2600", SIMULATED_MODEL, "p2"),
            ],
            &[info(SIMULATED_MODEL)],
        );
        assert_eq!(claims.placed(), vec![(0, "p2")]);
        assert_eq!(claims.unplaced_notes(), Vec::<String>::new());
    }

    /// The normalizer, the staged inventory and the simulated SDK's check
    /// all name the simulated camera by one product id.
    #[cfg(feature = "simulation")]
    #[test]
    fn the_simulated_product_is_the_one_the_simulated_sdk_supports() {
        assert_eq!(
            SIMULATED_PRODUCT,
            format!("{:04x}", zwo_rs::SIM_CAMERA_PRODUCT_ID)
        );
        assert!(sdk_supports(SIMULATED_PRODUCT));
    }

    #[cfg(feature = "simulation")]
    #[test]
    fn the_simulated_inventory_places_the_simulated_camera() {
        let infos = zwo_rs::Sdk::new().unwrap().cameras().unwrap();
        assert_eq!(infos[0].name, SIMULATED_MODEL);
        let claims = Claims::new(
            NORMALIZER,
            SERVICE,
            default_usb_source().scan(),
            sdk_cameras(&infos),
        );
        assert_eq!(claims.placed(), vec![(0, "simulated-usbv3-0:1")]);
    }

    #[test]
    fn the_listing_shows_no_sdk_id_and_carries_no_override_name() {
        let mut config = Config::default();
        config.devices.insert(
            "1915d5081b090900".to_string(),
            DeviceOverride {
                name: Some("Main".to_string()),
                description: None,
            },
        );
        let infos = [info("ZWO ASI178MM")];
        let claims = claims_for(vec![record("178c", "ASI178MM", "p1")], &infos);
        let listing = listing(&config, &claims, &infos);
        assert_eq!(listing.rows[0].sdk_id, None);
        assert_eq!(listing.block.as_ref().unwrap()[0].name, None);
        assert!(
            listing.paste_notes.iter().any(|n| n.starts_with(
                "The devices override 1915d5081b090900 cannot be matched to a camera without \
                 opening one"
            )),
            "{:?}",
            listing.paste_notes
        );
    }

    fn entry(name: Option<&str>) -> UsbDeviceEntry {
        UsbDeviceEntry {
            device_number: 0,
            usb_port: "pci-0000:c1:00.4-usbv3-0:1.4".to_string(),
            name: name.map(str::to_string),
            description: None,
        }
    }

    #[test]
    fn a_placeholder_answers_to_the_placeholder_unique_id_of_its_port() {
        assert_eq!(
            placeholder(&entry(None), String::new()).unique_id(),
            "placeholder:zwo-camera:pci-0000:c1:00.4-usbv3-0:1.4"
        );
    }

    #[test]
    fn a_placeholders_name_names_the_port_unless_the_entry_names_it() {
        assert_eq!(
            placeholder(&entry(None), String::new()).static_name(),
            "ZWO camera on pci-0000:c1:00.4-usbv3-0:1.4 (placeholder)"
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
