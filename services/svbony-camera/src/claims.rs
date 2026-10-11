//! svbony-camera's half of the device claims (docs/services/svbony-camera.md
//! U1-U9).
//!
//! Which SDK models it knows on the bus (the normalizer, U2), where its USB
//! scan comes from (U7), and the `doctor --devices` listing (U8). The join, the
//! list's validation, port order and the listing's layout are
//! `rusty-photon-doctor-checks`' `claims` module, shared with the other camera
//! drivers.

use rusty_photon_doctor_checks::claims::{
    BlockEntry, Claims, Listing, ListingRow, Normalizer, SdkCamera,
};
use rusty_photon_doctor_checks::UsbScan;
use svbony_rs::CameraInfo;

use crate::config::Config;

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

/// What this driver knows about `SVBony` cameras on the bus.
pub const NORMALIZER: Normalizer<'static> = Normalizer {
    sdk: SDK_NAME,
    vendor: VENDOR,
    models: MODELS,
    not_cameras: &[],
};

/// The SDK's cameras as the join reads them.
///
/// `CameraSN` is not offered as a serial: no `SVBony` camera has been seen
/// publishing a USB serial, so how the bus would spell one beside `CameraSN` is
/// unknown — and a serial both sides carry in different spellings would refuse
/// the right camera (U2).
#[must_use]
pub fn sdk_cameras(infos: &[CameraInfo]) -> Vec<SdkCamera> {
    infos
        .iter()
        .map(|info| SdkCamera {
            model: info.friendly_name.clone(),
            serial: None,
        })
        .collect()
}

/// Where this driver's USB scan comes from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum UsbSource {
    /// The host's collector — the only source a release build has.
    Host,
    /// One working record per fabricated camera (U7).
    #[cfg(feature = "simulation")]
    Simulated,
    /// The staged document in this file, read again at every scan (U7).
    #[cfg(feature = "simulation")]
    Staged(std::path::PathBuf),
}

impl Default for UsbSource {
    /// The host in a release build; the fabricated cameras' records in a
    /// simulation build, which never scans the host.
    fn default() -> Self {
        #[cfg(feature = "simulation")]
        {
            Self::Simulated
        }
        #[cfg(not(feature = "simulation"))]
        {
            Self::Host
        }
    }
}

impl UsbSource {
    /// The source a simulation build's hidden `--usb-inventory` flag selects
    /// (U7): the staged document in `path`, read now so a file that cannot be
    /// read or stages a state no collector could produce refuses the start;
    /// without the flag, the simulated cameras' records.
    ///
    /// # Errors
    ///
    /// Why the staged document is unusable, naming the file.
    #[cfg(feature = "simulation")]
    pub fn from_flag(path: Option<std::path::PathBuf>) -> Result<Self, String> {
        let Some(path) = path else {
            return Ok(Self::Simulated);
        };
        rusty_photon_doctor_checks::facts::StagedUsbInventory::load(&path)?;
        Ok(Self::Staged(path))
    }

    /// Take the scan. Blocking: the host collector reads sysfs or shells out
    /// under its deadline.
    ///
    /// # Errors
    ///
    /// Why the scan could not run — the collector's reason, or a staged
    /// document that cannot be read or stages a failure.
    pub fn scan(&self) -> Result<UsbScan, String> {
        match self {
            Self::Host => rusty_photon_doctor_checks::scan_usb(),
            #[cfg(feature = "simulation")]
            Self::Simulated => Ok(simulated_inventory()),
            #[cfg(feature = "simulation")]
            Self::Staged(path) => rusty_photon_doctor_checks::facts::StagedUsbInventory::load(path)
                .and_then(rusty_photon_doctor_checks::facts::StagedUsbInventory::into_scan),
        }
    }
}

/// One working record per camera the `svbony-rs` simulation fabricates:
/// `f266:9a0a` on port `simulated-usbv3-0:<n>` for the camera at SDK index
/// `n - 1`.
#[cfg(feature = "simulation")]
fn simulated_inventory() -> UsbScan {
    UsbScan {
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

/// The key a camera's `devices` override is read under: its `CameraSN`, or
/// `noserial-<index>` for a camera that reports none.
#[must_use]
pub fn override_key(info: &CameraInfo, index: usize) -> String {
    if info.serial.is_empty() {
        format!("noserial-{index}")
    } else {
        info.serial.clone()
    }
}

/// What `doctor --devices` prints for `config`, the join in `claims` and the
/// SDK's cameras (U8).
#[must_use]
pub fn listing(config: &Config, claims: &Claims<'_>, infos: &[CameraInfo]) -> Listing {
    let placed = claims.placed();
    let rows = placed
        .iter()
        .map(|&(index, port)| {
            let info = infos.get(index);
            ListingRow {
                port: port.to_string(),
                model: info.map(|i| i.friendly_name.clone()).unwrap_or_default(),
                sdk_id: info
                    .filter(|i| !i.serial.is_empty())
                    .map(|i| i.serial.clone()),
                usb_serial: claims.record_on(port).and_then(|r| r.serial.clone()),
                device: config.usb_devices.as_ref().map_or_else(
                    || index.to_string(),
                    |list| {
                        list.iter().find(|e| e.usb_port == port).map_or_else(
                            || "not listed".to_string(),
                            |e| e.device_number.to_string(),
                        )
                    },
                ),
            }
        })
        .collect::<Vec<_>>();

    let mut notes = claims.unplaced_notes();
    let block = match &config.usb_devices {
        Some(list) => {
            let mut entries: Vec<BlockEntry> = list
                .iter()
                .map(|e| BlockEntry {
                    device_number: e.device_number,
                    usb_port: e.usb_port.clone(),
                    name: e.name.clone(),
                    description: e.description.clone(),
                })
                .collect();
            entries.sort_by_key(|e| e.device_number);
            Some(entries)
        }
        None if placed.is_empty() => None,
        None => Some(no_list_block(config, &placed, infos, &mut notes)),
    };
    if rows.len() > 1 {
        notes.push(
            "Cannot tell which row is which camera? Unplug one and run this again: the port \
             that disappears is the one you unplugged."
                .to_string(),
        );
    }
    Listing {
        sdk: SDK_NAME.to_string(),
        vendor: VENDOR.to_string(),
        service: SERVICE.to_string(),
        rows,
        block,
        notes,
    }
}

/// The block that pins what the no-list default serves: every placed camera,
/// numbered in port order, with the names its `devices` override gives it.
fn no_list_block(
    config: &Config,
    placed: &[(usize, &str)],
    infos: &[CameraInfo],
    notes: &mut Vec<String>,
) -> Vec<BlockEntry> {
    let mut matched_keys = Vec::new();
    let block: Vec<BlockEntry> = placed
        .iter()
        .zip(0_u32..)
        .map(|(&(index, port), device_number)| {
            let key = infos.get(index).map(|info| override_key(info, index));
            let found = key.as_ref().and_then(|k| config.devices.get(k));
            if found.is_some() {
                matched_keys.extend(key);
            }
            BlockEntry {
                device_number,
                usb_port: port.to_string(),
                name: found.and_then(|o| o.name.clone()),
                description: found.and_then(|o| o.description.clone()),
            }
        })
        .collect();
    if placed
        .iter()
        .zip(0_usize..)
        .any(|(&(index, _), position)| index != position)
    {
        notes.push(
            "Pasting this block renumbers cameras: without a list the service serves them in \
             SDK order, and the block numbers them in port order. Check rp's \
             cameras[].device_number when you paste it."
                .to_string(),
        );
    }
    for key in config.devices.keys().filter(|k| !matched_keys.contains(k)) {
        notes.push(format!(
            "The devices override {key} matches no camera placed on a port; move its fields \
             into the right entry by hand."
        ));
    }
    if !config.devices.is_empty() {
        notes.push(format!(
            "Delete the devices map from {SERVICE}.json when you paste this block: the block \
             carries its names, and a devices map beside a usb_devices list refuses the start."
        ));
    }
    block
}

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
mod tests {
    use super::*;
    use crate::config::{DeviceOverride, UsbDeviceEntry};
    use rusty_photon_doctor_checks::UsbDevice;

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
    fn a_camera_without_a_serial_is_keyed_by_its_index() {
        assert_eq!(override_key(&info("SV605CC", ""), 2), "noserial-2");
        assert_eq!(override_key(&info("SV605CC", "SN"), 2), "SN");
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
            listing.rows[0].device, "1",
            "the SDK-order number it is served at"
        );
        assert!(
            listing
                .notes
                .iter()
                .any(|n| n.starts_with("Pasting this block renumbers")),
            "{:?}",
            listing.notes
        );
        assert!(
            listing
                .notes
                .iter()
                .any(|n| n.contains("devices override stale")),
            "{:?}",
            listing.notes
        );
        assert!(
            listing
                .notes
                .iter()
                .any(|n| n.starts_with("Delete the devices map from svbony-camera.json")),
            "{:?}",
            listing.notes
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
            UsbSource::Simulated.scan(),
            sdk_cameras(&infos),
        );
        assert_eq!(claims.placed(), vec![(0, "simulated-usbv3-0:1")]);
    }
}
