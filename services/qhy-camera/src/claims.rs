//! qhy-camera's half of the device claims (docs/services/qhy-camera.md
//! U1-U10).
//!
//! Which QHY models it knows on the bus (the normalizer, U2), what the SDK
//! reports of each camera before any open, its simulated USB scan (U7), what
//! `doctor --devices` can say about each camera (U8), and the placeholders it
//! registers for a camera and for its declared wheel (U4, U10). The join, the
//! list's validation, port order, the scan source and the listing are
//! `rusty-photon-doctor-checks`' `claims` module, and the placeholder Camera is
//! `rusty-photon-driver`'s, shared with the other camera drivers.

use std::collections::BTreeMap;

pub use rusty_photon_doctor_checks::claims::UsbSource;
use rusty_photon_doctor_checks::claims::{
    self, CameraFacts, Claims, DisplayNames, ListEntry, Listing, Normalizer, SdkCamera,
};
use rusty_photon_driver::claims::{Placeholder, PlaceholderCamera};

use tracing::warn;

use crate::config::{Config, UsbDeviceEntry};
use crate::config_actions::QhyCameraDriver;
use crate::filterwheel::PlaceholderFilterWheel;

/// The service name: the config file's stem and the placeholder `UniqueID`'s
/// middle field.
pub const SERVICE: &str = "qhy-camera";

/// The SDK's display name, for messages.
pub const SDK_NAME: &str = "QHY";

/// QHYCCD's USB vendor id, as the inventory spells it.
pub const VENDOR: &str = "1618";

/// The model the `qhyccd-rs` simulation fabricates.
#[cfg(feature = "simulation")]
pub const SIMULATED_MODEL: &str = "QHY178M-Simulated";

/// The product id the simulated camera is placed by and staged under: chosen
/// for the simulation, since no bus record of it exists (U7).
#[cfg(feature = "simulation")]
const SIMULATED_PRODUCT: &str = "f178";

/// Each QHY model this driver has seen on the bus, by the name
/// `GetQHYCCDModel` gives it, with the product id it enumerates under —
/// observed pairs only, since a guess here serves the wrong camera at the
/// wrong number. The QHY178M was read on the dev box (Fedora 44, `x86_64`,
/// SDK 26.06.04), its bus record from sysfs; the QHY600M and the QHY5III678M
/// on `rig2` (Windows 11, SDK 24.01.09), from its Plug and Play properties; all on
/// 2026-10-11, none of them publishing a USB serial.
const OBSERVED: [(&str, &str); 3] = [
    ("QHY178M", "c179"),
    ("QHY600M", "c601"),
    ("QHY5III678M", "0679"),
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

/// The firmware loaders this driver has seen on the bus: a QHY camera
/// enumerates under its loader's product id until its firmware is loaded —
/// every power-up passes through it — and a loader is no camera the SDK can
/// report. Listed so that a loader's record is never a camera's, not even by
/// elimination, which would otherwise pair it with an unrecognised camera
/// whose own record is missing. The QHY178M's is `c178` (the dev box, 2026-10-11).
const LOADERS: &[&str] = &["c178"];

/// What this driver knows about QHY cameras on the bus.
///
/// The QHY SDK has no passive check of a product id, so every one of QHYCCD's
/// product ids that is neither the table's nor a loader's could be a camera's
/// (U2).
pub const NORMALIZER: Normalizer<'static> = Normalizer {
    sdk: SDK_NAME,
    vendor: VENDOR,
    models: MODELS,
    not_cameras: LOADERS,
    sdk_check: None,
};

/// One camera as the SDK reports it before any open: its SDK id
/// (`GetQHYCCDId`) and its model (`GetQHYCCDModel`, which takes the id).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Enumerated {
    pub id: String,
    pub model: String,
}

/// Each camera `sdk` enumerated, in SDK order, with its model. Blocking: a
/// model read is an SDK call. Nothing is opened.
///
/// A camera whose model cannot be read is joined as a model this driver does
/// not know ([`enumerated`]), so one unreadable camera — the guide camera a
/// list leaves out, say — costs no other camera its number.
#[must_use]
pub fn enumerate(sdk: &qhyccd_rs::Sdk) -> Vec<Enumerated> {
    sdk.cameras()
        .map(|camera| enumerated(camera.id(), camera.get_model()))
        .collect()
}

/// The camera `id`, with the model its read returned — or, when the read
/// failed, a model no normalizer entry can name, which the join treats as an
/// unrecognised one (U2, U3) and the messages show as unreadable.
fn enumerated(id: &str, model: Result<String, qhyccd_rs::QHYError>) -> Enumerated {
    let model = model.unwrap_or_else(|error| {
        warn!(camera = %id, %error, "the QHY SDK could not read the camera's model");
        format!("<model unreadable: {id}>")
    });
    Enumerated {
        id: id.to_string(),
        model,
    }
}

/// The SDK's cameras as the join reads them: their models alone. No QHY
/// camera has been seen publishing a USB serial, so the serial in an SDK id is
/// offered to nothing (U2).
#[must_use]
pub fn sdk_cameras(cameras: &[Enumerated]) -> Vec<SdkCamera> {
    cameras
        .iter()
        .map(|camera| SdkCamera {
            model: camera.model.clone(),
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
/// fabricated camera's record, since a simulation build never scans the host
/// (U7).
#[cfg(feature = "simulation")]
#[must_use]
pub fn default_usb_source() -> UsbSource {
    UsbSource::Simulated(simulated_inventory())
}

/// The source a simulation build's hidden `--usb-inventory` flag selects
/// (U7): the staged document in `path`, or without it the simulated camera's
/// record.
///
/// # Errors
///
/// Why the staged document is unusable, naming the file.
#[cfg(feature = "simulation")]
pub fn usb_source_from_flag(path: Option<std::path::PathBuf>) -> Result<UsbSource, String> {
    UsbSource::from_flag(path, simulated_inventory())
}

/// One working record for the one camera the `qhyccd-rs` simulation
/// fabricates: `1618:f178` on port `simulated-usbv3-0:1`, publishing no USB
/// serial.
#[cfg(feature = "simulation")]
fn simulated_inventory() -> rusty_photon_doctor_checks::UsbScan {
    rusty_photon_doctor_checks::UsbScan {
        devices: vec![rusty_photon_doctor_checks::UsbDevice {
            vendor: VENDOR.to_string(),
            product: SIMULATED_PRODUCT.to_string(),
            model: Some(SIMULATED_MODEL.to_string()),
            port: Some("simulated-usbv3-0:1".to_string()),
            serial: None,
        }],
        faults: Vec::new(),
    }
}

/// What a placeholder for `entry` says about itself: the `name` and
/// `unique_id` of a camera's or a wheel's, and the rest from the entry and
/// this driver.
fn about(entry: &UsbDeviceEntry, name: String, unique_id: String, reason: String) -> Placeholder {
    Placeholder {
        name,
        unique_id,
        usb_port: entry.usb_port.clone(),
        reason,
        driver_info: crate::DRIVER_INFO.to_string(),
        driver_version: env!("CARGO_PKG_VERSION").to_string(),
        driver_code: claims::PLACEHOLDER_DRIVER_CODE,
    }
}

/// The placeholder that holds `entry`'s camera number for `reason` (U4).
#[must_use]
pub fn placeholder(entry: &UsbDeviceEntry, reason: String) -> PlaceholderCamera<QhyCameraDriver> {
    let name = entry
        .name
        .clone()
        .unwrap_or_else(|| claims::placeholder_name(SDK_NAME, &entry.usb_port));
    PlaceholderCamera::new(about(
        entry,
        name,
        claims::placeholder_unique_id(SERVICE, &entry.usb_port),
        reason,
    ))
}

/// The placeholder that holds `entry`'s declared wheel number for `reason`,
/// its camera's (U10). It never carries the entry's `name`, which is the
/// camera's.
#[must_use]
pub fn placeholder_wheel(entry: &UsbDeviceEntry, reason: String) -> PlaceholderFilterWheel {
    PlaceholderFilterWheel::new(about(
        entry,
        format!(
            "{SDK_NAME} filter wheel on {} (placeholder)",
            entry.usb_port
        ),
        claims::placeholder_unique_id(SERVICE, &format!("CFW-{}", entry.usb_port)),
        reason,
    ))
}

/// What `doctor --devices` prints for `config`, the join in `claims` and the
/// SDK's `cameras` (U8).
///
/// A QHY camera's SDK id is read without an open, and it is the key its
/// `devices` override is read under, so every override is matched and carried
/// — a camera whose override names its filters gets a declared wheel. Whether
/// any other camera has a wheel needs an open, which `--devices` never makes,
/// so the text above the block says how to declare one.
#[must_use]
pub fn listing(config: &Config, claims: &Claims<'_>, cameras: &[Enumerated]) -> Listing {
    let facts: Vec<CameraFacts> = cameras
        .iter()
        .map(|camera| CameraFacts {
            model: camera.model.clone(),
            sdk_id: Some(camera.id.clone()),
            override_key: Some(camera.id.clone()),
        })
        .collect();
    let list: Option<Vec<_>> = config
        .usb_devices
        .as_ref()
        .map(|list| list.iter().map(ListEntry::block_entry).collect());
    let overrides: BTreeMap<String, DisplayNames> = config
        .devices
        .iter()
        .map(|(key, o)| {
            (
                key.clone(),
                DisplayNames {
                    name: o.name.clone(),
                    description: o.description.clone(),
                    filter_names: o.filter_names.clone(),
                },
            )
        })
        .collect();
    let mut listing = claims.listing(&facts, list.as_deref(), &overrides);
    listing.wheels = true;
    // The shared note on a block that renumbers cameras and their wheels
    // already says to check the wheels' numbers; the text above the block says
    // why `--devices` cannot see a wheel.
    let wheel_numbers_noted = listing
        .notes
        .iter()
        .any(|note| note.contains("filter_wheels[].device_number"));
    if list.is_none() && listing.block.is_some() && !wheel_numbers_noted {
        // A camera whose override names no filters may still have a wheel the
        // service serves now, which the block cannot declare, so the wheel
        // numbers the block gives may not be the served ones even when no
        // camera moves.
        listing.notes.push(
            "Check rp's filter_wheels[].device_number when you paste this block: without a \
             list the service numbers the wheels its probe finds in SDK order, and --devices \
             cannot see which cameras have one, so the wheel numbers you declare may not be \
             the ones it serves now."
                .to_string(),
        );
    }
    listing
}

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
mod tests {
    use super::*;
    use crate::config::DeviceOverride;
    use ascom_alpaca::api::Device;
    use rusty_photon_doctor_checks::{UsbDevice, UsbScan};

    fn camera(id: &str, model: &str) -> Enumerated {
        Enumerated {
            id: id.to_string(),
            model: model.to_string(),
        }
    }

    fn record(product: &str, model: &str, port: &str) -> UsbDevice {
        UsbDevice {
            vendor: "1618".to_string(),
            product: product.to_string(),
            model: Some(model.to_string()),
            port: Some(port.to_string()),
            serial: None,
        }
    }

    fn claims_for(devices: Vec<UsbDevice>, cameras: &[Enumerated]) -> Claims<'static> {
        Claims::new(
            NORMALIZER,
            SERVICE,
            Ok(UsbScan {
                devices,
                faults: Vec::new(),
            }),
            sdk_cameras(cameras),
        )
    }

    /// `rig2` as it was read on 2026-10-11: both cameras placed by the
    /// product ids their models enumerate under.
    #[test]
    fn the_observed_cameras_are_placed_by_the_models_the_sdk_gives_them() {
        let claims = claims_for(
            vec![
                record("c601", "QHY600U3G20-20230614", "PCIROOT(0)#USB(14)#USB(1)"),
                record("0679", "QHY678U3G20-20230106", "PCIROOT(0)#USB(14)#USB(3)"),
            ],
            &[
                camera("QHY5III678M-4a94be45b27d3aa79", "QHY5III678M"),
                camera("QHY600M-3b1ea54688eb9f9d7", "QHY600M"),
            ],
        );
        assert_eq!(
            claims.placed(),
            vec![
                (1, "PCIROOT(0)#USB(14)#USB(1)"),
                (0, "PCIROOT(0)#USB(14)#USB(3)"),
            ]
        );
    }

    #[test]
    fn the_dev_box_qhy178m_is_placed_by_its_product_id() {
        let claims = claims_for(
            vec![record("c179", "Q178-Cool", "pci-0000:c1:00.4-usbv3-0:1.3")],
            &[camera("QHY178M-222b16468c5966524", "QHY178M")],
        );
        assert_eq!(claims.placed(), vec![(0, "pci-0000:c1:00.4-usbv3-0:1.3")]);
    }

    #[test]
    fn no_serial_is_offered_to_the_join() {
        assert_eq!(
            sdk_cameras(&[camera("QHY178M-222b16468c5966524", "QHY178M")])[0].serial,
            None
        );
    }

    #[test]
    fn a_camera_whose_firmware_is_not_loaded_is_not_placed_on_its_loaders_record() {
        let claims = claims_for(
            vec![record("c178", "Q178", "p1")],
            &[camera("QHY178M-222b16468c5966524", "QHY178M")],
        );
        assert_eq!(claims.placed(), Vec::<(usize, &str)>::new());
    }

    #[test]
    fn two_cameras_of_one_model_are_look_alikes() {
        let claims = claims_for(
            vec![
                record("c601", "QHY600U3G20", "p1"),
                record("c601", "QHY600U3G20", "p2"),
            ],
            &[
                camera("QHY600M-a", "QHY600M"),
                camera("QHY600M-b", "QHY600M"),
            ],
        );
        assert_eq!(claims.placed(), Vec::<(usize, &str)>::new());
    }

    #[test]
    fn a_firmware_loaders_record_is_no_camera_even_by_elimination() {
        let claims = claims_for(
            vec![record("c178", "Q178", "p1")],
            &[camera("QHY268M-1", "QHY268M")],
        );
        assert_eq!(claims.placed(), Vec::<(usize, &str)>::new());
    }

    #[test]
    fn an_unrecognised_camera_is_still_paired_by_elimination_with_a_lone_unknown_record() {
        let claims = claims_for(
            vec![record("c268", "QHY268", "p1")],
            &[camera("QHY268M-1", "QHY268M")],
        );
        assert_eq!(claims.placed(), vec![(0, "p1")]);
    }

    #[test]
    fn a_camera_whose_model_cannot_be_read_is_joined_as_an_unknown_model() {
        let camera = enumerated(
            "QHY600M-1",
            Err(qhyccd_rs::QHYError::Sdk { op: "get_model" }),
        );
        assert_eq!(camera.id, "QHY600M-1");
        assert_eq!(camera.model, "<model unreadable: QHY600M-1>");
        assert_eq!(
            NORMALIZER.models.iter().find(|(m, _)| *m == camera.model),
            None
        );
    }

    /// The guide camera a list leaves out cannot have its model read: the
    /// imaging camera is still placed, and the guide camera is not paired by
    /// elimination with its record, whose product id is a known model's.
    #[test]
    fn an_unreadable_camera_costs_no_other_camera_its_port() {
        let claims = claims_for(
            vec![
                record("c601", "QHY600U3G20", "p1"),
                record("0679", "QHY678U3G20", "p2"),
            ],
            &[
                enumerated(
                    "QHY5III678M-1",
                    Err(qhyccd_rs::QHYError::Sdk { op: "get_model" }),
                ),
                camera("QHY600M-1", "QHY600M"),
            ],
        );
        assert_eq!(claims.placed(), vec![(1, "p1")]);
    }

    /// The placeholder at the unreadable camera's listed port names the
    /// unreadable model, not a camera the SDK does not report.
    #[test]
    fn an_unreadable_cameras_port_names_its_unreadable_model() {
        let claims = claims_for(
            vec![
                record("c601", "QHY600U3G20", "p1"),
                record("0679", "QHY678U3G20", "p2"),
            ],
            &[
                enumerated(
                    "QHY5III678M-1",
                    Err(qhyccd_rs::QHYError::Sdk { op: "get_model" }),
                ),
                camera("QHY600M-1", "QHY600M"),
            ],
        );
        let rusty_photon_doctor_checks::claims::Resolution::Placeholder(reason) =
            claims.resolve("p2")
        else {
            panic!("p2 resolved to a camera");
        };
        assert!(
            reason
                .contains("a model this driver knows, but the QHY SDK reports none of that model"),
            "{reason}"
        );
        assert!(
            reason.contains("(<model unreadable: QHY5III678M-1>)"),
            "{reason}"
        );
    }

    #[test]
    fn a_no_list_block_warns_that_the_wheel_numbers_may_not_be_the_served_ones() {
        let cameras = [camera("QHY178M-222b16468c5966524", "QHY178M")];
        let claims = claims_for(vec![record("c179", "Q178-Cool", "p1")], &cameras);
        let wheel_note = |listing: &Listing| {
            listing
                .notes
                .iter()
                .any(|n| n.starts_with("Check rp's filter_wheels[].device_number"))
        };
        assert!(wheel_note(&listing(&Config::default(), &claims, &cameras)));
        let listed = Config {
            usb_devices: Some(vec![entry(None)]),
            ..Config::default()
        };
        assert!(
            !wheel_note(&listing(&listed, &claims, &cameras)),
            "a list's block reproduces its wheel numbers"
        );
    }

    /// `rig2`'s cameras, whose SDK order is not their port order, with the
    /// QHY600M's filters named: pasting renumbers a camera and its wheel, which
    /// one note says, not two.
    #[test]
    fn a_block_that_renumbers_the_wheels_says_to_check_them_once() {
        let cameras = [
            camera("QHY5III678M-4a94be45b27d3aa79", "QHY5III678M"),
            camera("QHY600M-3b1ea54688eb9f9d7", "QHY600M"),
        ];
        let claims = claims_for(
            vec![
                record("c601", "QHY600U3G20-20230614", "PCIROOT(0)#USB(14)#USB(1)"),
                record("0679", "QHY678U3G20-20230106", "PCIROOT(0)#USB(14)#USB(3)"),
            ],
            &cameras,
        );
        let mut config = Config::default();
        config.devices.insert(
            "QHY600M-3b1ea54688eb9f9d7".to_string(),
            DeviceOverride {
                filter_names: Some(vec!["L".to_string(), "R".to_string()]),
                ..DeviceOverride::default()
            },
        );

        let listing = listing(&config, &claims, &cameras);

        let wheel_notes: Vec<_> = listing
            .notes
            .iter()
            .filter(|n| n.contains("filter_wheels[].device_number"))
            .collect();
        assert_eq!(wheel_notes.len(), 1, "{:#?}", listing.notes);
        assert!(
            wheel_notes[0].starts_with("Pasting this block renumbers cameras and their filter"),
            "{}",
            wheel_notes[0]
        );
    }

    #[cfg(feature = "simulation")]
    #[test]
    fn the_simulated_inventory_places_the_simulated_camera() {
        let sdk = qhyccd_rs::Sdk::new().unwrap();
        let cameras = enumerate(&sdk);
        assert_eq!(cameras, [camera("SIM-QHY178M", SIMULATED_MODEL)]);
        let claims = Claims::new(
            NORMALIZER,
            SERVICE,
            default_usb_source().scan(),
            sdk_cameras(&cameras),
        );
        assert_eq!(claims.placed(), vec![(0, "simulated-usbv3-0:1")]);
    }

    #[test]
    fn the_listing_shows_the_sdk_id_and_carries_an_overrides_names_and_wheel() {
        let mut config = Config::default();
        config.devices.insert(
            "QHY178M-222b16468c5966524".to_string(),
            DeviceOverride {
                name: Some("Main".to_string()),
                description: None,
                filter_names: Some(vec!["L".to_string(), "R".to_string()]),
            },
        );
        let cameras = [camera("QHY178M-222b16468c5966524", "QHY178M")];
        let claims = claims_for(vec![record("c179", "Q178-Cool", "p1")], &cameras);
        let listing = listing(&config, &claims, &cameras);
        assert_eq!(
            listing.rows[0].sdk_id.as_deref(),
            Some("QHY178M-222b16468c5966524")
        );
        assert!(listing.wheels);
        let block = listing.block.unwrap();
        assert_eq!(block[0].name.as_deref(), Some("Main"));
        assert_eq!(block[0].filter_wheel_number, Some(0));
        assert_eq!(
            block[0].filter_names,
            Some(vec!["L".to_string(), "R".to_string()])
        );
    }

    #[test]
    fn with_a_list_the_listing_reproduces_its_declared_wheels() {
        let config = Config {
            usb_devices: Some(vec![entry(None)]),
            ..Config::default()
        };
        let cameras = [camera("QHY178M-222b16468c5966524", "QHY178M")];
        let claims = claims_for(vec![record("c179", "Q178-Cool", "p1")], &cameras);
        let block = listing(&config, &claims, &cameras).block.unwrap();
        assert_eq!(block[0].filter_wheel_number, Some(0));
        assert_eq!(block[0].filter_names, Some(vec!["L".to_string()]));
    }

    fn entry(name: Option<&str>) -> UsbDeviceEntry {
        UsbDeviceEntry {
            device_number: 0,
            usb_port: "pci-0000:c1:00.4-usbv3-0:1.3".to_string(),
            name: name.map(str::to_string),
            description: None,
            filter_wheel_number: Some(0),
            filter_names: Some(vec!["L".to_string()]),
        }
    }

    #[test]
    fn a_placeholder_answers_to_the_placeholder_unique_id_of_its_port() {
        assert_eq!(
            placeholder(&entry(None), String::new()).unique_id(),
            "placeholder:qhy-camera:pci-0000:c1:00.4-usbv3-0:1.3"
        );
    }

    #[test]
    fn a_placeholders_name_names_the_port_unless_the_entry_names_it() {
        assert_eq!(
            placeholder(&entry(None), String::new()).static_name(),
            "QHY camera on pci-0000:c1:00.4-usbv3-0:1.3 (placeholder)"
        );
        assert_eq!(
            placeholder(&entry(Some("Main")), String::new()).static_name(),
            "Main"
        );
    }

    #[test]
    fn a_placeholder_wheel_has_a_unique_id_and_name_of_its_own() {
        let wheel = placeholder_wheel(&entry(Some("Main")), String::new());
        assert_eq!(
            wheel.unique_id(),
            "placeholder:qhy-camera:CFW-pci-0000:c1:00.4-usbv3-0:1.3"
        );
        assert_eq!(
            wheel.static_name(),
            "QHY filter wheel on pci-0000:c1:00.4-usbv3-0:1.3 (placeholder)",
            "the entry's name is its camera's"
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
    async fn a_placeholder_wheel_refuses_a_connect_with_the_device_claims_code() {
        let error = placeholder_wheel(&entry(None), "the USB scan failed".to_string())
            .set_connected(true)
            .await
            .unwrap_err();
        assert_eq!(error.code.raw(), claims::PLACEHOLDER_ERROR_CODE);
        assert!(error.message.ends_with("the USB scan failed"), "{error}");
    }

    #[tokio::test]
    async fn placeholders_answer_this_drivers_driver_info() {
        assert_eq!(
            placeholder(&entry(None), String::new())
                .driver_info()
                .await
                .unwrap(),
            crate::DRIVER_INFO
        );
        assert_eq!(
            placeholder_wheel(&entry(None), String::new())
                .driver_info()
                .await
                .unwrap(),
            crate::DRIVER_INFO
        );
    }
}
