//! The `hardware.*` check family (docs/services/doctor.md §Hardware): the
//! no-SDK device-surface checks.
//!
//! Judged over
//! [`HardwareFacts`](rusty_photon_doctor_checks::HardwareFacts) — staged
//! by the test seam, gathered read-only otherwise. One severity rule for
//! the family: `fail` when the unit will start at boot and hit the
//! problem, `warn` otherwise.

use std::path::PathBuf;

use rusty_photon_doctor_checks::{udev, HardwareFacts, Identity, PathKind, ProbeRequest};
use rusty_photon_server_config::doctor_toml::SerialMeta;
use serde_json::Value;

use crate::catalog;
use crate::checks::Context;
use crate::facts::{Platform, PlatformFacts};
use crate::report::{Check, Mode};
use crate::scan::ServiceScan;

/// The user every packaged unit runs as.
pub const SERVICE_USER: &str = "rusty-photon";

/// The qhy firmware helper's three artifacts — its own idempotency gate,
/// mirrored here.
///
/// The script is `services/qhy-camera/pkg/rusty-photon-qhy-firmware-install`;
/// `tests/tree_parity.rs` asserts the paths against it. Any subset is a
/// partial install that must re-converge, so the check wants all three.
pub const QHY_FIRMWARE_ARTIFACTS: [(&str, PathKind); 3] = [
    ("/lib/firmware/qhy", PathKind::Dir),
    ("/usr/local/sbin/fxload", PathKind::File),
    ("/etc/udev/rules.d/85-qhyccd.rules", PathKind::File),
];

const QHY_FIRMWARE_HELPER: &str = "/usr/sbin/rusty-photon-qhy-firmware-install";

/// What the hardware gatherer should probe, derived from the catalog and
/// the scanned configs — the checks then judge exactly these answers.
#[must_use]
pub fn probe_request(scans: &[ServiceScan], facts: &PlatformFacts) -> ProbeRequest {
    let mut req = ProbeRequest {
        service_user: SERVICE_USER.to_string(),
        ..Default::default()
    };
    for scan in scans {
        if !participates(scan, facts) {
            continue;
        }
        if facts.platform != Platform::Windows {
            if let Some(path) = effective_serial_path(scan, facts.platform) {
                req.paths.push(PathBuf::from(path));
            }
        }
        // udev rules, firmware artifacts, and the service-user writability
        // judgment are Linux/systemd facts — probing them elsewhere is
        // syscall noise the checks would never read.
        if facts.platform != Platform::Linux {
            continue;
        }
        if let Some(rule) = catalog::udev_rule_for(scan.entry.name) {
            req.udev_rules.push(rule.file_name.to_string());
        }
        if scan.entry.name == "qhy-camera" {
            req.paths
                .extend(QHY_FIRMWARE_ARTIFACTS.iter().map(|(p, _)| PathBuf::from(p)));
        }
    }
    if facts.platform == Platform::Linux {
        if let Some(dir) = rp_data_directory(scans) {
            req.paths.push(PathBuf::from(dir));
        }
    }
    req
}

/// rp's configured `session.data_directory`, for the probe list.
fn rp_data_directory(scans: &[ServiceScan]) -> Option<String> {
    let rp = scans.iter().find(|s| s.entry.name == "rp")?;
    let view = crate::scan::view::<crate::scan::RpView>(rp)?.ok()?;
    view.session.and_then(|s| s.data_directory)
}

/// Run the family. No hardware facts (a staged scenario without a
/// `hardware` object) means no hardware story to judge — the family is
/// skipped, never probed underneath a mock.
#[must_use]
pub fn checks(ctx: &Context) -> Vec<Check> {
    let Some(hw) = &ctx.hardware else {
        return Vec::new();
    };
    let mut checks = Vec::new();
    for scan in &ctx.scans {
        if !participates(scan, &ctx.facts) {
            continue;
        }
        serial_node(ctx, hw, scan, &mut checks);
        usb_device(ctx, hw, scan, &mut checks);
        udev_rule(ctx, hw, scan, &mut checks);
        if scan.entry.name == "qhy-camera" {
            firmware_helper(ctx, hw, scan, &mut checks);
        }
    }
    checks
}

fn participates(scan: &ServiceScan, facts: &PlatformFacts) -> bool {
    scan.config_present() || facts.unit(&scan.entry.unit_name()).is_some()
}

/// The family's one severity rule.
fn fail_or_warn(
    ctx: &Context,
    scan: &ServiceScan,
    name: &str,
    detail: String,
    suggestion: Option<String>,
) -> Check {
    let enabled = ctx
        .facts
        .unit(&scan.entry.unit_name())
        .is_some_and(|u| u.enabled);
    if enabled {
        Check::fail(name, Some(svc(scan)), detail, suggestion)
    } else {
        Check::warn(name, Some(svc(scan)), detail, suggestion)
    }
}

fn svc(scan: &ServiceScan) -> String {
    scan.entry.name.to_string()
}

/// The device path the service will actually open: the config value at
/// the catalog pointer, else the platform default — `None` when the
/// service has no serial metadata or its transport gate points elsewhere.
fn effective_serial_path(scan: &ServiceScan, platform: Platform) -> Option<String> {
    let meta = scan.entry.serial.as_ref()?;
    if !gate_open(meta, scan.value()) {
        return None;
    }
    let configured = scan
        .value()
        .and_then(|v| v.pointer(&meta.pointer))
        .and_then(Value::as_str);
    Some(
        configured
            .unwrap_or(match platform {
                Platform::Windows => &meta.default_windows,
                Platform::Linux | Platform::Macos => &meta.default_unix,
            })
            .to_string(),
    )
}

/// A gated service participates unless its config explicitly selects
/// another transport — an absent config or gate key means the default
/// (usb) transport.
fn gate_open(meta: &SerialMeta, value: Option<&Value>) -> bool {
    let Some((pointer, wanted)) = &meta.gate else {
        return true;
    };
    value
        .and_then(|v| v.pointer(pointer))
        .and_then(Value::as_str)
        .is_none_or(|actual| actual == wanted)
}

// ---- hardware.serial-node / hardware.serial-access ----

fn serial_node(ctx: &Context, hw: &HardwareFacts, scan: &ServiceScan, checks: &mut Vec<Check>) {
    let Some(path) = effective_serial_path(scan, ctx.facts.platform) else {
        return;
    };
    if ctx.facts.platform == Platform::Windows {
        if hw.com_ports.iter().any(|p| p.eq_ignore_ascii_case(&path)) {
            checks.push(Check::ok(
                "hardware.serial-node",
                Some(svc(scan)),
                format!("serial port {path} is present"),
            ));
        } else {
            checks.push(fail_or_warn(
                ctx,
                scan,
                "hardware.serial-node",
                format!(
                    "serial port {path} is not among the host's COM ports ({}) — \
                     the service cannot open its device",
                    if hw.com_ports.is_empty() {
                        "none present".to_string()
                    } else {
                        hw.com_ports.join(", ")
                    }
                ),
                Some(format!(
                    "plug the device in, or point {} at the right port",
                    pointer_hint(scan)
                )),
            ));
        }
        return;
    }
    match hw.paths.get(&path) {
        None => checks.push(fail_or_warn(
            ctx,
            scan,
            "hardware.serial-node",
            format!(
                "serial device {path} does not exist — the device is unplugged, \
                 powered down, or the path is wrong"
            ),
            Some(format!(
                "plug the device in, or point {} at the right node",
                pointer_hint(scan)
            )),
        )),
        Some(facts) if facts.kind != PathKind::CharDevice => checks.push(fail_or_warn(
            ctx,
            scan,
            "hardware.serial-node",
            format!("{path} exists but is not a character device — not a serial port"),
            Some(format!(
                "point {} at a real device node",
                pointer_hint(scan)
            )),
        )),
        Some(facts) => {
            checks.push(Check::ok(
                "hardware.serial-node",
                Some(svc(scan)),
                format!("serial device {path} is present"),
            ));
            serial_access(ctx, hw, scan, &path, facts, checks);
        }
    }
}

/// Will the service user's `open()` succeed? Linux, packaged mode — the
/// judgment needs the unit's `SupplementaryGroups=` and the service user,
/// neither of which exists on a dev checkout. The verdict models what the
/// kernel grants: the union of the unit's groups and the account's own
/// memberships (systemd initializes the process group list from both), so
/// a node openable only via an account-level membership still passes —
/// with the granting mechanism named in the detail.
fn serial_access(
    ctx: &Context,
    hw: &HardwareFacts,
    scan: &ServiceScan,
    path: &str,
    node: &rusty_photon_doctor_checks::PathFacts,
    checks: &mut Vec<Check>,
) {
    if ctx.facts.platform != Platform::Linux || ctx.mode != Mode::Packaged {
        return;
    }
    let Some(unit) = ctx.facts.unit(&scan.entry.unit_name()) else {
        return;
    };
    let Some(user) = hw.service_user else {
        return;
    };
    let mut gids = vec![user.gid];
    gids.extend(
        unit.supplementary_groups
            .iter()
            .filter_map(|name| hw.groups.get(name).copied()),
    );
    let unit_identity = Identity {
        uid: user.uid,
        gids: gids.clone(),
    };
    gids.extend(
        hw.service_user_groups
            .iter()
            .filter_map(|name| hw.groups.get(name).copied()),
    );
    let identity = Identity {
        uid: user.uid,
        gids,
    };
    if identity.can_read_write(node) {
        let detail = if unit_identity.can_read_write(node) {
            format!("{path} is openable by the {SERVICE_USER} user")
        } else {
            format!(
                "{path} is openable by the {SERVICE_USER} user via its \
                 account-level{} membership — the unit declares no matching \
                 SupplementaryGroups= entry",
                hw.group_name(node.gid)
                    .map_or_else(|| " group".to_string(), |g| format!(" {g} group")),
            )
        };
        checks.push(Check::ok("hardware.serial-access", Some(svc(scan)), detail));
        return;
    }
    let owning_group = hw.group_name(node.gid);
    // Membership is missing only when the process's full group set (primary
    // + unit + account) does not hold the owning group — a held group that
    // still cannot open the node is a mode problem, not a membership one.
    let missing_membership = owning_group.is_some() && !identity.gids.contains(&node.gid);
    let detail = format!(
        "{path} (mode {:o}, uid {}, gid {}{}) is not openable by the \
         {SERVICE_USER} user — judged from ownership and mode, so ACLs are \
         invisible to this check",
        node.mode,
        node.uid,
        node.gid,
        owning_group
            .map(|g| format!(" = group {g}"))
            .unwrap_or_default(),
    );
    let suggestion = if missing_membership {
        // Packaged units all carry their SupplementaryGroups=; losing one
        // means a drop-in or hand-edit overrode it.
        owning_group.map(|g| {
            format!(
                "the unit confers no {g} membership — add SupplementaryGroups={g} \
                 to {} (packaged units ship it; check for drop-in overrides)",
                scan.entry.unit_name()
            )
        })
    } else {
        Some(format!(
            "fix the node's ownership or mode (udev rules set it at plug time — \
             see the hardware.udev-rule check for {})",
            scan.entry.name
        ))
    };
    checks.push(fail_or_warn(
        ctx,
        scan,
        "hardware.serial-access",
        detail,
        suggestion,
    ));
}

fn pointer_hint(scan: &ServiceScan) -> String {
    scan.entry.serial.as_ref().map_or_else(
        || scan.entry.config_file(),
        |meta| format!("{} in {}", meta.pointer, scan.entry.config_file()),
    )
}

// ---- hardware.usb-device ----

fn usb_device(ctx: &Context, hw: &HardwareFacts, scan: &ServiceScan, checks: &mut Vec<Check>) {
    let Some(usb) = &scan.entry.usb else {
        return;
    };
    // A serial service whose transport gate points elsewhere has no USB
    // device to expect either.
    if let Some(meta) = &scan.entry.serial {
        if !gate_open(meta, scan.value()) {
            return;
        }
    }
    let identity = describe_identity(usb);
    if hw.usb_present(&usb.vendor, usb.product.as_deref(), usb.model.as_deref()) {
        checks.push(Check::ok(
            "hardware.usb-device",
            Some(svc(scan)),
            format!("a USB device matching {identity} is present"),
        ));
    } else {
        checks.push(fail_or_warn(
            ctx,
            scan,
            "hardware.usb-device",
            format!(
                "no USB device matching {identity} is on the bus — the device is \
                 unplugged, unpowered, or behind a hub that dropped it"
            ),
            Some("check the cable, power, and hub; then re-run doctor".to_string()),
        ));
    }
}

fn describe_identity(usb: &rusty_photon_server_config::doctor_toml::UsbMeta) -> String {
    use std::fmt::Write as _;
    let mut identity = usb.vendor.clone();
    match &usb.product {
        Some(product) => {
            let _ = write!(identity, ":{product}");
        }
        None => identity.push_str(":*"),
    }
    if let Some(model) = &usb.model {
        let _ = write!(identity, " (\"{model}\")");
    }
    identity
}

// ---- hardware.udev-rule ----

fn udev_rule(ctx: &Context, hw: &HardwareFacts, scan: &ServiceScan, checks: &mut Vec<Check>) {
    if ctx.facts.platform != Platform::Linux || ctx.mode != Mode::Packaged {
        return;
    }
    let Some(rule) = catalog::udev_rule_for(scan.entry.name) else {
        return;
    };
    let Some(installed) = hw.udev_rules.get(rule.file_name) else {
        checks.push(fail_or_warn(
            ctx,
            scan,
            "hardware.udev-rule",
            format!(
                "{} is not installed in any udev rules directory — device nodes \
                 will keep root-only ownership and the service cannot open them",
                rule.file_name
            ),
            Some(format!(
                "reinstall the {} package (it ships the rule)",
                scan.entry.unit_name()
            )),
        ));
        return;
    };
    let unresolvable: Vec<String> = udev::group_assignments(installed)
        .into_iter()
        .filter(|g| !hw.groups.contains_key(g))
        .collect();
    if !unresolvable.is_empty() {
        checks.push(fail_or_warn(
            ctx,
            scan,
            "hardware.udev-rule",
            format!(
                "{} names GROUP= {} which is not present in /etc/group — udev \
                 silently drops the entire rule line on an unresolvable group, \
                 so the rule file's presence proves nothing (a group resolved \
                 purely via NSS is invisible to this check)",
                rule.file_name,
                unresolvable.join(", ")
            ),
            Some(format!(
                "create the group (`groupadd -r {}`), then replug the device or \
                 `udevadm trigger`",
                unresolvable.join("`, `groupadd -r ")
            )),
        ));
        return;
    }
    if installed != rule.content {
        checks.push(Check::warn(
            "hardware.udev-rule",
            Some(svc(scan)),
            format!(
                "the installed {} differs from the packaged rule — an operator \
                 override, or a stale copy from an older package",
                rule.file_name
            ),
            Some(
                "diff it against the packaged copy; overrides in /etc/udev/rules.d \
                 are legitimate but worth knowing about"
                    .to_string(),
            ),
        ));
        return;
    }
    checks.push(Check::ok(
        "hardware.udev-rule",
        Some(svc(scan)),
        format!(
            "{} is installed, its groups resolve, and it matches the packaged rule",
            rule.file_name
        ),
    ));
}

// ---- hardware.firmware-helper ----

fn firmware_helper(ctx: &Context, hw: &HardwareFacts, scan: &ServiceScan, checks: &mut Vec<Check>) {
    if ctx.facts.platform != Platform::Linux || ctx.mode != Mode::Packaged {
        return;
    }
    let missing: Vec<&str> = QHY_FIRMWARE_ARTIFACTS
        .iter()
        .filter(|(path, kind)| {
            let ok = hw.paths.get(*path).is_some_and(|f| {
                f.kind == *kind && (*path != "/usr/local/sbin/fxload" || f.mode & 0o111 != 0)
            });
            !ok
        })
        .map(|(path, _)| *path)
        .collect();
    if missing.is_empty() {
        checks.push(Check::ok(
            "hardware.firmware-helper",
            Some(svc(scan)),
            "camera firmware, fxload, and the SDK udev rules are installed".to_string(),
        ));
    } else if missing.len() == QHY_FIRMWARE_ARTIFACTS.len() {
        checks.push(fail_or_warn(
            ctx,
            scan,
            "hardware.firmware-helper",
            "camera firmware is not installed — QHY cameras cannot boot without \
             it, and it is never packaged (ADR-013)"
                .to_string(),
            Some(format!("run `{QHY_FIRMWARE_HELPER}` once as root")),
        ));
    } else {
        checks.push(fail_or_warn(
            ctx,
            scan,
            "hardware.firmware-helper",
            format!(
                "the firmware install is partial — missing: {} — any subset must \
                 re-converge before a camera can boot",
                missing.join(", ")
            ),
            Some(format!("re-run `{QHY_FIRMWARE_HELPER}` as root")),
        ));
    }
}

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
mod tests {
    use super::*;
    use crate::scan::scan_service;

    fn scan_with(name: &'static str, config: Option<&str>) -> ServiceScan {
        let dir = tempfile::tempdir().unwrap();
        let entry = catalog::entry(name).unwrap();
        if let Some(config) = config {
            std::fs::write(dir.path().join(entry.config_file()), config).unwrap();
        }
        scan_service(dir.path(), entry)
    }

    #[test]
    fn test_effective_path_prefers_config_and_falls_back_per_platform() {
        let configured = scan_with(
            "ppba-driver",
            Some(r#"{ "serial": { "port": "/dev/ttyUSB7" } }"#),
        );
        assert_eq!(
            effective_serial_path(&configured, Platform::Linux).as_deref(),
            Some("/dev/ttyUSB7")
        );
        let absent = scan_with("ppba-driver", None);
        assert_eq!(
            effective_serial_path(&absent, Platform::Linux).as_deref(),
            Some("/dev/ttyUSB0")
        );
        assert_eq!(
            effective_serial_path(&absent, Platform::Macos).as_deref(),
            Some("/dev/ttyUSB0"),
            "macOS shares the unix default"
        );
        assert_eq!(
            effective_serial_path(&absent, Platform::Windows).as_deref(),
            Some("COM3")
        );
        let no_serial = scan_with("sentinel", None);
        assert_eq!(effective_serial_path(&no_serial, Platform::Linux), None);
    }

    #[test]
    fn test_transport_gate_defaults_open_and_closes_on_udp() {
        let default_transport = scan_with("star-adventurer-gti", Some("{}"));
        assert_eq!(
            effective_serial_path(&default_transport, Platform::Linux).as_deref(),
            Some("/dev/ttyACM0"),
            "an absent gate key means the default (usb) transport"
        );
        let udp = scan_with(
            "star-adventurer-gti",
            Some(r#"{ "transport": { "kind": "udp", "address": "192.168.4.1", "port": 11880 } }"#),
        );
        assert_eq!(
            effective_serial_path(&udp, Platform::Linux),
            None,
            "a udp transport has no serial device — /transport/port is a UDP port there"
        );
        let usb = scan_with(
            "star-adventurer-gti",
            Some(r#"{ "transport": { "kind": "usb", "port": "/dev/mount" } }"#),
        );
        assert_eq!(
            effective_serial_path(&usb, Platform::Linux).as_deref(),
            Some("/dev/mount")
        );
    }

    #[test]
    fn test_usb_identity_description_shows_wildcards_and_models() {
        let ppba = catalog::entry("ppba-driver").unwrap().usb.as_ref().unwrap();
        assert_eq!(describe_identity(ppba), "0403:6015 (\"PPBA\")");
        let qhy = catalog::entry("qhy-camera").unwrap().usb.as_ref().unwrap();
        assert_eq!(describe_identity(qhy), "1618:*");
    }

    #[test]
    fn test_probe_request_covers_participating_services_only() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("ppba-driver.json"), "{}").unwrap();
        let facts = crate::facts::PlatformFacts {
            platform: Platform::Linux,
            units: vec![crate::facts::UnitFacts {
                name: "rusty-photon-qhy-camera".to_string(),
                enabled: true,
                condition_path: None,
                source_name: None,
                supplementary_groups: Vec::new(),
                active: None,
                failed: None,
                binary_path: None,
            }],
            polkit_grants_sentinel_restart: None,
            hardware: None,
            probe_hardware: false,
        };
        let scans: Vec<ServiceScan> = catalog::catalog()
            .iter()
            .map(|entry| scan_service(dir.path(), entry))
            .collect();
        let req = probe_request(&scans, &facts);
        assert!(
            req.paths.iter().any(|p| p.to_str() == Some("/dev/ttyUSB0")),
            "ppba-driver participates via its config file: {:?}",
            req.paths
        );
        assert!(
            req.paths
                .iter()
                .any(|p| p.to_str() == Some("/usr/local/sbin/fxload")),
            "qhy-camera participates via its unit, bringing the firmware artifacts"
        );
        assert_eq!(
            req.udev_rules,
            vec!["90-rusty-photon-qhy.rules".to_string()]
        );
        assert!(
            !req.paths.iter().any(|p| p.to_str() == Some("/dev/ttyACM0")),
            "services with neither config nor unit are not probed"
        );
    }
}
