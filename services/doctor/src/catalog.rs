//! The derived service catalog (docs/services/doctor.md §The derived
//! catalog).
//!
//! Each packaged service's `pkg/doctor.toml` is embedded at build time and
//! parsed once on first access. The embed list below is the one hand-typed
//! encoding of the service set inside doctor; it is kept honest by the
//! `catalog_matches_the_packaging_tree` test (Cargo runs, walks
//! `services/*/pkg`) and by each service's own parity test against its
//! config defaults.

use std::sync::LazyLock;

use rusty_photon_server_config::doctor_toml::{self, SerialMeta, ServerClass, UsbMeta};

/// One packaged service the doctor knows about.
#[derive(Debug, Clone)]
pub struct CatalogEntry {
    /// The service name — the `services/<name>` directory, the `<name>.json`
    /// config file, and the `rusty-photon-<name>` unit stem.
    pub name: &'static str,
    /// Which shared `server` shape the config uses.
    pub class: ServerClass,
    /// The port the service defaults to when its config omits one.
    pub default_port: u16,
    /// The service hard-requires a hand-written config and never
    /// self-creates one (docs/packaging.md's "config-gated" services:
    /// `calibrator-flats`, `focus-model`, `plate-solver`, `polar-align`,
    /// `session-runner`, `sky-survey-camera`). A
    /// `FileAbsent` scan is expected and unremarkable for these — the unit
    /// cannot start without an operator writing the file first, so it never
    /// serves plain HTTP the way a self-defaulting service would
    /// (docs/services/doctor.md §TLS and auth).
    pub config_gated: bool,
    /// Where the config keeps its serial device path, for the six serial
    /// drivers (docs/services/doctor.md §Hardware).
    pub serial: Option<SerialMeta>,
    /// The USB identity the device reports on the bus.
    pub usb: Option<UsbMeta>,
}

impl CatalogEntry {
    /// The platform-neutral unit stem: `rusty-photon-<name>` names the
    /// systemd unit, the Windows service, and the brew formula alike.
    #[must_use]
    pub fn unit_name(&self) -> String {
        format!("rusty-photon-{}", self.name)
    }

    /// The config file name inside the config directory.
    #[must_use]
    pub fn config_file(&self) -> String {
        format!("{}.json", self.name)
    }
}

/// The embedded `pkg/doctor.toml` files, alphabetical by service.
static RAW: &[(&str, &str)] = &[
    (
        "calibrator-flats",
        include_str!("../../calibrator-flats/pkg/doctor.toml"),
    ),
    ("dsd-fp2", include_str!("../../dsd-fp2/pkg/doctor.toml")),
    (
        "filemonitor",
        include_str!("../../filemonitor/pkg/doctor.toml"),
    ),
    (
        "focus-model",
        include_str!("../../focus-model/pkg/doctor.toml"),
    ),
    (
        "pa-falcon-rotator",
        include_str!("../../pa-falcon-rotator/pkg/doctor.toml"),
    ),
    (
        "pa-scops-oag",
        include_str!("../../pa-scops-oag/pkg/doctor.toml"),
    ),
    (
        "phd2-guider",
        include_str!("../../phd2-guider/pkg/doctor.toml"),
    ),
    (
        "planetarium-bridge",
        include_str!("../../planetarium-bridge/pkg/doctor.toml"),
    ),
    (
        "plate-solver",
        include_str!("../../plate-solver/pkg/doctor.toml"),
    ),
    (
        "polar-align",
        include_str!("../../polar-align/pkg/doctor.toml"),
    ),
    (
        "ppba-driver",
        include_str!("../../ppba-driver/pkg/doctor.toml"),
    ),
    (
        "qhy-camera",
        include_str!("../../qhy-camera/pkg/doctor.toml"),
    ),
    (
        "qhy-focuser",
        include_str!("../../qhy-focuser/pkg/doctor.toml"),
    ),
    ("rp", include_str!("../../rp/pkg/doctor.toml")),
    ("sentinel", include_str!("../../sentinel/pkg/doctor.toml")),
    (
        "session-runner",
        include_str!("../../session-runner/pkg/doctor.toml"),
    ),
    (
        "sky-survey-camera",
        include_str!("../../sky-survey-camera/pkg/doctor.toml"),
    ),
    (
        "star-adventurer-gti",
        include_str!("../../star-adventurer-gti/pkg/doctor.toml"),
    ),
    (
        "svbony-camera",
        include_str!("../../svbony-camera/pkg/doctor.toml"),
    ),
    ("ui-htmx", include_str!("../../ui-htmx/pkg/doctor.toml")),
    (
        "upbv2-driver",
        include_str!("../../upbv2-driver/pkg/doctor.toml"),
    ),
    (
        "zwo-camera",
        include_str!("../../zwo-camera/pkg/doctor.toml"),
    ),
    (
        "zwo-focuser",
        include_str!("../../zwo-focuser/pkg/doctor.toml"),
    ),
];

static CATALOG: LazyLock<Vec<CatalogEntry>> = LazyLock::new(|| {
    // A malformed embedded `doctor.toml` is a repo defect rather than a
    // runtime condition — `test_catalog_covers_every_embedded_service`
    // fails on it. Dropping the entry keeps one bad file from aborting
    // every doctor run.
    RAW.iter()
        .filter_map(|(name, content)| {
            let meta = doctor_toml::parse(content).ok()?;
            Some(CatalogEntry {
                name,
                class: meta.class,
                default_port: meta.port,
                config_gated: meta.config_gated,
                serial: meta.serial,
                usb: meta.usb,
            })
        })
        .collect()
});

/// The udev rules the camera/focuser packages ship, embedded for the
/// installed-content comparison and the `GROUP=` resolution check
/// (docs/services/doctor.md §Hardware).
///
/// sentinel's `50-*.rules` is a polkit rule, not udev, and stays out.
pub struct UdevRule {
    pub service: &'static str,
    /// The file name packages install (into the udev rules directory).
    pub file_name: &'static str,
    pub content: &'static str,
}

pub static UDEV_RULES: &[UdevRule] = &[
    UdevRule {
        service: "qhy-camera",
        file_name: "90-rusty-photon-qhy.rules",
        content: include_str!("../../qhy-camera/pkg/90-rusty-photon-qhy.rules"),
    },
    UdevRule {
        service: "svbony-camera",
        file_name: "90-rusty-photon-svbony.rules",
        content: include_str!("../../svbony-camera/pkg/90-rusty-photon-svbony.rules"),
    },
    UdevRule {
        service: "zwo-camera",
        file_name: "90-rusty-photon-zwo.rules",
        content: include_str!("../../zwo-camera/pkg/90-rusty-photon-zwo.rules"),
    },
    UdevRule {
        service: "zwo-focuser",
        file_name: "90-rusty-photon-zwo-focuser.rules",
        content: include_str!("../../zwo-focuser/pkg/90-rusty-photon-zwo-focuser.rules"),
    },
];

/// The shipped udev rule of one service, when it ships one.
#[must_use]
pub fn udev_rule_for(service: &str) -> Option<&'static UdevRule> {
    UDEV_RULES.iter().find(|r| r.service == service)
}

/// Every packaged service, alphabetical.
#[must_use]
pub fn catalog() -> &'static [CatalogEntry] {
    &CATALOG
}

/// Look up a service by name.
#[must_use]
pub fn entry(name: &str) -> Option<&'static CatalogEntry> {
    catalog().iter().find(|e| e.name == name)
}

/// Look up a service by its `rusty-photon-<name>` unit stem (with or
/// without a `.service` suffix).
#[must_use]
pub fn entry_for_unit(unit: &str) -> Option<&'static CatalogEntry> {
    let stem = unit.strip_suffix(".service").unwrap_or(unit);
    let name = stem.strip_prefix("rusty-photon-")?;
    entry(name)
}

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
mod tests {
    use std::collections::HashSet;

    use rusty_photon_doctor_checks::facts::{HardwareFacts, UsbDevice};

    use super::*;

    /// The catalog skips an embedded `doctor.toml` it cannot parse, so this
    /// is the check that stops a malformed one from shipping: without it a
    /// service would just go missing from every report.
    #[test]
    fn test_catalog_covers_every_embedded_service() {
        for (name, content) in RAW {
            if let Err(e) = doctor_toml::parse(content) {
                panic!("embedded {name}/pkg/doctor.toml is invalid: {e}");
            }
        }
        assert_eq!(catalog().len(), RAW.len());
    }

    #[test]
    fn test_catalog_parses_and_ports_are_unique() {
        let catalog = catalog();
        assert!(!catalog.is_empty());
        let ports: HashSet<u16> = catalog.iter().map(|e| e.default_port).collect();
        assert_eq!(ports.len(), catalog.len(), "default ports must be unique");
    }

    #[test]
    fn test_catalog_knows_every_class() {
        assert_eq!(entry("qhy-focuser").unwrap().class, ServerClass::Alpaca);
        assert_eq!(entry("sentinel").unwrap().class, ServerClass::Core);
        assert_eq!(entry("rp").unwrap().class, ServerClass::Advertising);
        assert_eq!(entry("qhy-focuser").unwrap().default_port, 11113);
    }

    /// The six services with no sensible default config (docs/packaging.md
    /// §Installing) declare `config_gated`; nothing else does. Drift here
    /// means `tls.absent`/`auth.absent` either wrongly nags a hard-gated
    /// service or wrongly stays silent about a self-defaulting one whose
    /// config was deleted.
    #[test]
    fn test_config_gated_matches_the_known_set() {
        const GATED: &[&str] = &[
            "calibrator-flats",
            "focus-model",
            "plate-solver",
            "polar-align",
            "session-runner",
            "sky-survey-camera",
        ];
        for entry in catalog() {
            assert_eq!(
                entry.config_gated,
                GATED.contains(&entry.name),
                "{}: config_gated should be {}",
                entry.name,
                GATED.contains(&entry.name)
            );
        }
    }

    #[test]
    fn test_unit_name_round_trips() {
        let entry = entry_for_unit("rusty-photon-qhy-focuser.service").unwrap();
        assert_eq!(entry.name, "qhy-focuser");
        assert_eq!(entry.unit_name(), "rusty-photon-qhy-focuser");
        assert_eq!(entry.config_file(), "qhy-focuser.json");
        assert!(entry_for_unit("ssh.service").is_none());
    }

    /// The USB checks read identity from doctor.toml while udev grants
    /// access by rule — one declared vendor drifting from its rule would
    /// make the check assert a device the rule never covers.
    #[test]
    fn test_rule_shipping_services_declare_the_vendor_their_rule_matches() {
        for rule in UDEV_RULES {
            let entry = entry(rule.service).unwrap_or_else(|| {
                panic!("{} ships a rule but is not in the catalog", rule.service)
            });
            let declared = entry.usb.as_ref().unwrap_or_else(|| {
                panic!(
                    "{} ships a udev rule but declares no usb_vendor",
                    rule.service
                )
            });
            let matched = rusty_photon_doctor_checks::udev::vendor_matches(rule.content);
            assert_eq!(
                matched,
                vec![declared.vendor.clone()],
                "{}: doctor.toml usb_vendor vs ATTRS{{idVendor}} in {}",
                rule.service,
                rule.file_name
            );
        }
    }

    /// Serial metadata reaches the catalog intact — the per-service parity
    /// tests own the values; this guards the plumbing.
    #[test]
    fn test_serial_metadata_is_plumbed_through() {
        let ppba = entry("ppba-driver").unwrap().serial.as_ref().unwrap();
        assert_eq!(ppba.pointer, "/serial/port");
        assert_eq!(ppba.gate, None);
        let gti = entry("star-adventurer-gti")
            .unwrap()
            .serial
            .as_ref()
            .unwrap();
        assert_eq!(
            gti.gate,
            Some(("/transport/kind".to_string(), "usb".to_string()))
        );
        assert!(entry("sentinel").unwrap().serial.is_none());
        assert!(entry("sentinel").unwrap().usb.is_none());
    }

    /// One device as the fleet's buses actually report it: the Pi rig's
    /// sysfs `product` for the four it carries, rig2's
    /// `DEVPKEY_Device_BusReportedDeviceDesc` for the UPBv2.
    struct ObservedDevice {
        service: &'static str,
        vendor: &'static str,
        product: &'static str,
        descriptor: &'static str,
    }

    /// Transcribed from the fleet, never from a datasheet or a protocol
    /// table. `usb_model` is matched as a substring of `descriptor` and of
    /// nothing else, so a model taken from a vendor document matches no
    /// device on any bus, and the check's only way to report that is to call
    /// a present device unplugged. The names a device answers to over its
    /// own protocol are no guide: the UPBv2 replies `UPB2_OK` to `P#` while
    /// announcing itself to the USB host as `UPBv2 revA`.
    const OBSERVED: &[ObservedDevice] = &[
        ObservedDevice {
            service: "dsd-fp2",
            vendor: "2e8a",
            product: "000a",
            descriptor: "Deep Sky Dad FP2",
        },
        ObservedDevice {
            service: "pa-falcon-rotator",
            vendor: "0403",
            product: "6015",
            descriptor: "Falcon Rotator",
        },
        ObservedDevice {
            service: "pa-scops-oag",
            vendor: "0403",
            product: "6015",
            descriptor: "Scops OAG",
        },
        ObservedDevice {
            service: "ppba-driver",
            vendor: "0403",
            product: "6015",
            descriptor: "PPBADV Gen2C",
        },
        ObservedDevice {
            service: "upbv2-driver",
            vendor: "0403",
            product: "6015",
            descriptor: "UPBv2 revA",
        },
    ];

    /// The fleet's devices as a gathered USB inventory, minus the one named.
    fn observed_bus_without(skip: &str) -> HardwareFacts {
        HardwareFacts {
            usb: OBSERVED
                .iter()
                .filter(|d| d.service != skip)
                .map(|d| UsbDevice {
                    vendor: d.vendor.to_string(),
                    product: d.product.to_string(),
                    model: Some(d.descriptor.to_string()),
                })
                .collect(),
            ..HardwareFacts::default()
        }
    }

    /// The gatherer reads real descriptors at runtime, but no test can
    /// reach a bus, so the table above is the checked-in record of what the
    /// fleet reports — and it guards nothing unless every declaring service
    /// appears in it.
    #[test]
    fn test_every_service_declaring_a_usb_model_has_an_observed_descriptor() {
        for entry in catalog() {
            let Some(usb) = entry.usb.as_ref() else {
                continue;
            };
            assert_eq!(
                usb.model.is_some(),
                OBSERVED.iter().any(|d| d.service == entry.name),
                "{}: a declared usb_model and an observed descriptor go together",
                entry.name
            );
        }
    }

    /// The comparison doctor actually runs, against the bus the fleet
    /// actually presents. A `usb_model` that is not a substring of its own
    /// device's descriptor fails here rather than on a rig at dusk.
    #[test]
    fn test_every_declared_usb_model_matches_its_own_device_on_the_bus() {
        let bus = observed_bus_without("");
        for device in OBSERVED {
            let usb = entry(device.service)
                .unwrap_or_else(|| panic!("{} is not in the catalog", device.service))
                .usb
                .as_ref()
                .unwrap_or_else(|| panic!("{} declares no USB identity", device.service));
            assert!(
                bus.usb_present(&usb.vendor, usb.product.as_deref(), usb.model.as_deref()),
                "{}: declared {}:{} usb_model {:?} is not in the descriptor its \
                 device reports, {:?}",
                device.service,
                usb.vendor,
                usb.product.as_deref().unwrap_or("*"),
                usb.model.as_deref().unwrap_or(""),
                device.descriptor
            );
        }
    }

    /// Four of these five are FTDI `0403:6015`, so the product string is the
    /// only thing telling them apart. A model that also matched a sibling
    /// would have doctor call a powerbox present because a rotator is.
    #[test]
    fn test_a_declared_usb_model_rejects_its_siblings_behind_the_same_bridge() {
        for device in OBSERVED {
            let usb = entry(device.service).unwrap().usb.as_ref().unwrap();
            let siblings = observed_bus_without(device.service);
            assert!(
                !siblings.usb_present(&usb.vendor, usb.product.as_deref(), usb.model.as_deref()),
                "{}: usb_model {:?} also matches another device on the fleet's bus",
                device.service,
                usb.model.as_deref().unwrap_or("")
            );
        }
    }
}
