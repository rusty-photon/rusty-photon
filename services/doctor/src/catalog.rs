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
    /// `calibrator-flats`, `plate-solver`, `polar-align`,
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

    /// The five services with no sensible default config (docs/packaging.md
    /// §Installing) declare `config_gated`; nothing else does. Drift here
    /// means `tls.absent`/`auth.absent` either wrongly nags a hard-gated
    /// service or wrongly stays silent about a self-defaulting one whose
    /// config was deleted.
    #[test]
    fn test_config_gated_matches_the_known_set() {
        const GATED: &[&str] = &[
            "calibrator-flats",
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
}
