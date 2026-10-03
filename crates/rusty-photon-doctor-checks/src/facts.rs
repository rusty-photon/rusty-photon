//! [`HardwareFacts`]: everything the hardware checks look at, gathered
//! once, read-only.
//!
//! The struct serializes so doctor's `--platform-facts` test seam can
//! stage any host state on any OS; parsing is permissive
//! (`#[serde(default)]`, unknown fields tolerated) per the report-side
//! convention — facts cross the doctor↔service binary boundary from D5 on.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use tracing::debug;

/// What a probed path turned out to be.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "kebab-case")]
pub enum PathKind {
    CharDevice,
    Dir,
    File,
    /// Anything else (block device, socket, fifo) — present, but never
    /// what a serial-node or firmware check wants.
    #[default]
    #[serde(other)]
    Other,
}

/// `stat` results for one probed path. Ownership and mode are Unix facts;
/// on Windows they stay zero and no check reads them there.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct PathFacts {
    pub kind: PathKind,
    /// Permission bits (`st_mode & 0o7777`).
    #[serde(default)]
    pub mode: u32,
    #[serde(default)]
    pub uid: u32,
    #[serde(default)]
    pub gid: u32,
}

/// One device on the USB bus.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct UsbDevice {
    /// `idVendor`, four lowercase hex digits.
    pub vendor: String,
    /// `idProduct`, four lowercase hex digits.
    #[serde(default)]
    pub product: String,
    /// The product string the device reports (`iProduct`), when the
    /// platform exposes one — the discriminator for devices behind generic
    /// bridge chips.
    #[serde(default)]
    pub model: Option<String>,
    /// The device's port path in the platform's **native spelling**:
    /// `1-4.2` on Linux, a `PCIROOT(…)` location path on Windows, the
    /// location id on macOS. It names the socket, not the device, which is
    /// what makes it usable as an ownership key.
    ///
    /// `Option` here is a compatibility shim for staged fixtures written
    /// before this field existed, **not** a runtime state: a gathered
    /// candidate record without a port never becomes a `UsbDevice` — it is
    /// reported as a [`UsbFault`] — because a port-less device is
    /// indistinguishable from one whose port simply did not match.
    #[serde(default)]
    pub port: Option<String>,
    /// The serial the device publishes on the bus, when it publishes one.
    /// Many cameras publish none — an absent serial is normal and says
    /// nothing about whether the device has an identity elsewhere (a
    /// vendor SDK may expose one the bus never sees).
    #[serde(default)]
    pub serial: Option<String>,
}

/// A record the USB scan found but could not count as a working device.
///
/// The platform reports it not working (on Windows, a non-zero problem
/// code — including the placeholder Windows leaves for a device whose
/// enumeration failed), or its identity or port could not be read.
///
/// A fault is information for the operator, never a scan failure: it is
/// kept out of [`HardwareFacts::usb`], so nothing selects or opens it, and
/// every working device beside it is inventoried as usual.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct UsbFault {
    /// The platform's own name for the record, so an operator can find it:
    /// the Windows instance id, the sysfs entry, the macOS node name.
    pub record: String,
    /// `idVendor`, four lowercase hex digits, when the record names one.
    #[serde(default)]
    pub vendor: Option<String>,
    /// `idProduct`, four lowercase hex digits, when it could be read.
    #[serde(default)]
    pub product: Option<String>,
    /// The product string the device published on the bus, when it did.
    #[serde(default)]
    pub model: Option<String>,
    /// Where the record sits, in whatever spelling the platform offered:
    /// the native port when there is one, otherwise a location hint (the
    /// `ACPI(…)` chain on Windows).
    #[serde(default)]
    pub location: Option<String>,
    /// Why the record is not a working device.
    pub reason: String,
}

/// What a collector read off the bus: the working devices, and the records
/// it could not count among them.
#[derive(Debug, Default)]
struct UsbScan {
    devices: Vec<UsbDevice>,
    faults: Vec<UsbFault>,
}

/// The service user's identity from the host's user database.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct UserFacts {
    pub uid: u32,
    pub gid: u32,
}

/// Everything the hardware checks look at. Every map is keyed by what was
/// probed; an absent key means "probed, not there" — the gatherer records
/// only what exists, and checks treat absence as the finding.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct HardwareFacts {
    /// `stat` results per probed path (serial nodes, firmware artifacts,
    /// data directories). Absent key = the path does not exist.
    #[serde(default)]
    pub paths: BTreeMap<String, PathFacts>,
    /// Present COM port names (Windows). Empty means the host has none
    /// **only** when [`Self::com_ports_unavailable`] is `None`; ask
    /// [`Self::com_port_present`], which answers for both.
    #[serde(default)]
    pub com_ports: Vec<String>,
    /// Why the COM-port listing could not be read, when it could not
    /// (Windows only; always `None` elsewhere). `Some` makes
    /// [`Self::com_ports`] meaningless rather than empty: a listing that
    /// failed has no opinion about which ports exist. A registry value that
    /// names no port is skipped rather than failing the listing, so there
    /// is no fault class.
    ///
    /// Absent from a staged fixture means the listing succeeded, so every
    /// fixture written before this field existed keeps its meaning.
    #[serde(default)]
    pub com_ports_unavailable: Option<String>,
    /// The host's USB inventory: the devices that are alive and working.
    /// Empty means an idle bus **only** when [`Self::usb_unavailable`] is
    /// `None`.
    #[serde(default)]
    pub usb: Vec<UsbDevice>,
    /// Records the scan found but could not count as working devices —
    /// reported, never inventoried. Absent from a staged fixture means none.
    #[serde(default)]
    pub usb_faults: Vec<UsbFault>,
    /// Why the USB scan could not run, when it could not. `Some` makes
    /// [`Self::usb`] meaningless rather than empty: a collector that failed,
    /// timed out, or returned output it could not parse has no opinion
    /// about what is on the bus. A single record that is not a working
    /// device is a [`UsbFault`] instead, and costs nothing else.
    ///
    /// Absent from a staged fixture means the scan succeeded, so every
    /// fixture written before this field existed keeps its meaning.
    #[serde(default)]
    pub usb_unavailable: Option<String>,
    /// gid per referenced group name. Absent key = the group does not
    /// exist — which is exactly what makes a udev `GROUP=` unresolvable.
    #[serde(default)]
    pub groups: BTreeMap<String, u32>,
    /// The `rusty-photon` service user, when it exists.
    #[serde(default)]
    pub service_user: Option<UserFacts>,
    /// Group names whose member list names the service user — its
    /// account-level supplementary memberships. A service process holds
    /// the union of these and its unit's `SupplementaryGroups=`, so
    /// access checks must credit both.
    #[serde(default)]
    pub service_user_groups: Vec<String>,
    /// Content of the **effective** installed copy of each expected udev
    /// rule file (`/etc/udev/rules.d` shadows `/run`, then `/usr/lib`,
    /// then `/lib` — udev's own precedence). Absent key = not installed.
    #[serde(default)]
    pub udev_rules: BTreeMap<String, String>,
}

impl HardwareFacts {
    /// Whether any present USB device matches the given identity: vendor,
    /// plus product when asked, plus `model` as a product-string substring
    /// when asked.
    ///
    /// `None` when the inventory is unavailable — a scan that could not run
    /// says nothing about whether the device is plugged in, and answering
    /// `false` would send an operator to check a cable over a host fault.
    #[must_use]
    pub fn usb_present(
        &self,
        vendor: &str,
        product: Option<&str>,
        model: Option<&str>,
    ) -> Option<bool> {
        if self.usb_unavailable.is_some() {
            return None;
        }
        Some(self.usb.iter().any(|d| {
            d.vendor == vendor
                && product.is_none_or(|p| d.product == p)
                && model.is_none_or(|m| d.model.as_deref().is_some_and(|dm| dm.contains(m)))
        }))
    }

    /// Whether the host lists the named COM port, compared without regard
    /// to ASCII case, as Windows compares device names.
    ///
    /// `None` when the listing is unavailable — a listing that could not
    /// be read says nothing about which ports exist, and answering `false`
    /// would send an operator to plug a device in over a host fault.
    #[must_use]
    pub fn com_port_present(&self, name: &str) -> Option<bool> {
        if self.com_ports_unavailable.is_some() {
            return None;
        }
        Some(self.com_ports.iter().any(|p| p.eq_ignore_ascii_case(name)))
    }

    /// The first fault whose identity matches, by the same rules as
    /// [`Self::usb_present`] — a device that is on the bus but not working.
    /// A fault that does not name the declared field never matches it.
    #[must_use]
    pub fn usb_fault_matching(
        &self,
        vendor: &str,
        product: Option<&str>,
        model: Option<&str>,
    ) -> Option<&UsbFault> {
        self.usb_faults.iter().find(|f| {
            f.vendor.as_deref() == Some(vendor)
                && product.is_none_or(|p| f.product.as_deref() == Some(p))
                && model.is_none_or(|m| f.model.as_deref().is_some_and(|fm| fm.contains(m)))
        })
    }

    /// The group name behind a gid, when the gid belongs to a gathered
    /// group — for diagnostics ("the node is group-owned by `dialout`").
    #[must_use]
    pub fn group_name(&self, gid: u32) -> Option<&str> {
        self.groups
            .iter()
            .find(|(_, g)| **g == gid)
            .map(|(name, _)| name.as_str())
    }
}

/// A USB inventory staged from a JSON document instead of read from the
/// host — the simulation-build affordance in `docs/services/doctor.md`.
///
/// Parsed rather than validated: the two states a collector can produce are
/// the two this type has, so a document describing neither is rejected at the
/// boundary and never reaches a consumer as a plausible-looking inventory.
#[cfg(feature = "mock")]
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StagedUsbInventory {
    /// The bus as staged: the working devices, every one carrying a port
    /// (a gathered candidate without one is a fault, not a device), and the
    /// records a collector would have reported as faults.
    Scan {
        devices: Vec<UsbDevice>,
        faults: Vec<UsbFault>,
    },
    /// A scan that failed, carrying the reason a collector would have given.
    Unavailable(String),
}

/// The wire shape of a staged inventory: the three inventory fields of
/// [`HardwareFacts`] under their own names, so the `hardware` object of a
/// facts file captured from a real rig stages unchanged. Every other key in
/// that object is ignored.
#[cfg(feature = "mock")]
#[derive(Debug, Deserialize)]
struct StagedDocument {
    /// `Option` to tell an *explicitly* empty bus (`"usb": []`, a state
    /// every collector can report) from a document that never mentioned the
    /// bus at all. Both deserialize to no devices; only one of them meant
    /// to.
    ///
    /// The reader rejects an explicit `null`, which neither of those is.
    /// `usb_unavailable` deliberately does not — there, `null` is how a
    /// *successful* scan serializes, because [`HardwareFacts`] holds it as
    /// an `Option` where it holds `usb` as a `Vec`.
    #[serde(default, deserialize_with = "devices_or_absent")]
    usb: Option<Vec<UsbDevice>>,
    /// Absent means none: a scan that found no faults serializes `[]`, and
    /// a capture from before faults existed meant the same.
    #[serde(default)]
    usb_faults: Vec<UsbFault>,
    #[serde(default)]
    usb_unavailable: Option<String>,
}

/// Read `usb` as a list that is present or absent, never null.
///
/// `Option::deserialize` folds `null` and a missing key into the same `None`,
/// and a null list is neither of the two things this document can mean: a
/// collector reports devices or reports a failure, never a nulled bus. Taking
/// the value first is what makes the difference visible.
#[cfg(feature = "mock")]
fn devices_or_absent<'de, D>(deserializer: D) -> Result<Option<Vec<UsbDevice>>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let value = serde_json::Value::deserialize(deserializer)?;
    if value.is_null() {
        return Err(serde::de::Error::custom(
            "`usb` is null; write `[]` for an empty bus, or omit the key and stage \
             `usb_unavailable` for a failed scan",
        ));
    }
    serde_json::from_value(value)
        .map(Some)
        .map_err(serde::de::Error::custom)
}

#[cfg(feature = "mock")]
impl TryFrom<StagedDocument> for StagedUsbInventory {
    type Error = String;

    fn try_from(document: StagedDocument) -> Result<Self, Self::Error> {
        // A document naming neither key states nothing, and the whole point
        // of this type is that nothing and empty are different answers. An
        // empty bus stays expressible, but has to be said out loud — and
        // faults alone do not say it, because a scan that found only faults
        // still reports its (empty) device list.
        if document.usb.is_none() && document.usb_unavailable.is_none() {
            return Err(
                "states neither a device list nor a failure; write `\"usb\": []` for an \
                 empty bus, or `usb_unavailable` with a reason for a failed scan"
                    .to_string(),
            );
        }
        let usb = document.usb.unwrap_or_default();
        let faults = document.usb_faults;
        match document.usb_unavailable {
            // A failed scan has no opinion about what is on the bus, so the
            // gatherer pairs the marker with empty lists. A document
            // claiming both would let a scenario assert on devices or faults
            // that a failed scan could never have reported.
            Some(_) if !usb.is_empty() || !faults.is_empty() => Err(
                "names both a failure and a device or fault list; a failed scan reports no \
                 devices and no faults"
                    .to_string(),
            ),
            // A collector that failed always says why — doctor prints the
            // reason to send an operator at the host fault rather than at a
            // cable, and a blank one would report a failure it cannot explain.
            Some(reason) if reason.trim().is_empty() => {
                Err("names a failure with no reason; a collector always reports one".to_string())
            }
            Some(reason) => Ok(Self::Unavailable(reason)),
            None => {
                for device in &usb {
                    check_staged_device(device)?;
                }
                for fault in &faults {
                    check_staged_fault(fault)?;
                }
                Ok(Self::Scan {
                    devices: usb,
                    faults,
                })
            }
        }
    }
}

/// Reject a staged device no collector could have put in the inventory.
#[cfg(feature = "mock")]
fn check_staged_device(device: &UsbDevice) -> Result<(), String> {
    let identity = format!("device {}:{}", device.vendor, device.product);
    // Blank is not the same absence as `None`, and it is the more dangerous
    // one: an empty field matches nothing and reads like a device that
    // simply did not match. No collector emits one — a candidate is a
    // candidate because it has a vendor id, one with an unreadable product
    // is reported as a fault, and every port spelling has at least one
    // component.
    if device.vendor.trim().is_empty() || device.product.trim().is_empty() {
        return Err(format!(
            "{identity} is missing a vendor or product id; a collector reports both for \
             every device in the inventory, and a record it cannot read is a fault"
        ));
    }
    if device.port.as_deref().is_none_or(|p| p.trim().is_empty()) {
        return Err(format!(
            "{identity} has no port; a record without one is a fault, not a device, so \
             stage it under `usb_faults` to get that outcome"
        ));
    }
    check_staged_text(&identity, "model", device.model.as_deref())?;
    check_staged_text(&identity, "serial", device.serial.as_deref())?;
    check_staged_text(&identity, "port", device.port.as_deref())?;
    check_staged_id(&identity, "vendor", &device.vendor)?;
    check_staged_id(&identity, "product", &device.product)
}

/// Reject a staged fault no collector could have reported. A fault is
/// often exactly the record whose identity or location could not be read,
/// so only its name and its reason are required — but whatever it does
/// carry follows the device rules, because a collector fills it the same
/// way.
#[cfg(feature = "mock")]
fn check_staged_fault(fault: &UsbFault) -> Result<(), String> {
    let identity = format!("fault {:?}", fault.record);
    // Doctor prints both: the record so the operator can find it, the
    // reason so they know what is wrong with it.
    for (field, value) in [("record", &fault.record), ("reason", &fault.reason)] {
        if value.trim().is_empty() {
            return Err(format!(
                "{identity} has a blank `{field}`; a collector names every fault it reports \
                 and says why"
            ));
        }
        check_staged_text(&identity, field, Some(value))?;
    }
    check_staged_text(&identity, "model", fault.model.as_deref())?;
    check_staged_text(&identity, "location", fault.location.as_deref())?;
    if let Some(vendor) = &fault.vendor {
        check_staged_id(&identity, "vendor", vendor)?;
    }
    if let Some(product) = &fault.product {
        check_staged_id(&identity, "product", product)?;
    }
    Ok(())
}

/// An optional descriptor-like field: `None` is how a collector reports
/// what it could not read, so a present value is neither blank nor padded.
#[cfg(feature = "mock")]
fn check_staged_text(identity: &str, field: &str, value: Option<&str>) -> Result<(), String> {
    let Some(value) = value else {
        return Ok(());
    };
    // A descriptor a collector could not read is `None`, never `Some("")`
    // — an empty string is the absence of a name wearing the shape of one,
    // and it would match a `usb_model` substring check no better than
    // `null` while looking like a device that reported something.
    if value.trim().is_empty() {
        return Err(format!(
            "{identity} has a blank `{field}`; a collector omits what it could not read, so \
             write `null` or leave the key out"
        ));
    }
    // Every collector stores what the platform reported with no padding
    // around it: the sysfs read is trimmed, each `LocationPaths` element is
    // trimmed before the `PCIROOT(` one is selected, and a macOS location id
    // is a single whitespace-split token. So a padded value is unreachable —
    // and it compares unequal to the same value without the padding, which
    // is precisely the silent no-match the port key exists to rule out.
    // Rejected rather than trimmed: silently rewriting a document hides the
    // mistake instead of reporting it.
    if value != value.trim() {
        return Err(format!(
            "{identity} has a padded `{field}` ({value:?}); a collector reports no padding, \
             and a padded value compares unequal to the same one without it"
        ));
    }
    Ok(())
}

/// `UsbDevice` documents both ids as four lowercase hex digits, and all
/// three collectors deliver exactly that: sysfs reports it, the Windows
/// instance id is lowercased on the way in, and the macOS reader accepts
/// nothing else. The mistake this catches is real and quiet — an id copied
/// from `Get-PnpDevice` output reads `PID_C601`, and `"C601"` compares
/// unequal to `"c601"` forever.
#[cfg(feature = "mock")]
fn check_staged_id(identity: &str, field: &str, value: &str) -> Result<(), String> {
    if value != value.trim() {
        return Err(format!(
            "{identity} has a padded `{field}` ({value:?}); a collector reports no padding, \
             and a padded value compares unequal to the same one without it"
        ));
    }
    if !is_usb_id(value) {
        return Err(format!(
            "{identity} has a `{field}` of {value:?}, which is not the four lowercase hex \
             digits every collector reports"
        ));
    }
    Ok(())
}

/// Four lowercase hex digits — the one spelling every collector reports a
/// vendor or product id in.
#[cfg(any(feature = "mock", windows, test))]
fn is_usb_id(value: &str) -> bool {
    value.len() == 4
        && value
            .chars()
            .all(|c| c.is_ascii_hexdigit() && !c.is_ascii_uppercase())
}

#[cfg(feature = "mock")]
impl StagedUsbInventory {
    /// Read a staged inventory from a JSON file.
    ///
    /// # Errors
    ///
    /// Returns a message if the file cannot be read, is not valid JSON, or
    /// describes a state no collector could produce.
    pub fn load(path: &Path) -> Result<Self, String> {
        let content = std::fs::read_to_string(path).map_err(|e| {
            format!(
                "could not read staged USB inventory {}: {e}",
                path.display()
            )
        })?;
        let document: StagedDocument = serde_json::from_str(&content)
            .map_err(|e| format!("staged USB inventory {} is invalid: {e}", path.display()))?;
        Self::try_from(document).map_err(|e| format!("staged USB inventory {} {e}", path.display()))
    }

    /// The collector-shaped result this document stands in for.
    fn into_scan(self) -> Result<UsbScan, String> {
        match self {
            Self::Scan { devices, faults } => Ok(UsbScan { devices, faults }),
            Self::Unavailable(reason) => Err(reason),
        }
    }
}

/// The request-scoped part of a gather: which paths to `stat`, which udev
/// rule files to read, which user to look up — derived by callers from
/// their catalog and configs.
///
/// The rest of [`HardwareFacts`] is host-wide inventory gathered
/// unconditionally, because the checks match against it rather than ask
/// for specific entries: the USB bus and COM-port lists (a check asks "is
/// my device among these"), and the whole (small) group database — the
/// checks resolve gids the request could not have anticipated (a node's
/// owning group comes from the distro's own udev defaults, and an
/// operator-edited rule can name any group).
#[derive(Debug, Clone, Default)]
pub struct ProbeRequest {
    pub paths: Vec<PathBuf>,
    /// udev rule file names (not paths — the gatherer searches the rules
    /// directories in precedence order).
    pub udev_rules: Vec<String>,
    /// The service user to look up.
    pub service_user: String,
    /// A staged USB inventory replacing the host scan. `None` — always, in a
    /// release build, where the field does not exist — means scan the host.
    #[cfg(feature = "mock")]
    pub staged_usb: Option<StagedUsbInventory>,
}

/// Gather hardware facts from the running host, read-only.
///
/// A path, group or rule file that cannot be read degrades to absence (a
/// path with a `debug!` trail); absence does not yet tell "not there" from
/// "unreadable". The two host-wide listings do tell them apart: a USB scan
/// or a Windows COM-port listing that could not be read is recorded as
/// unavailable ([`HardwareFacts::usb_unavailable`],
/// [`HardwareFacts::com_ports_unavailable`]), because a failed listing read
/// as an empty one turns a fault on the host into "plug the device in".
#[must_use]
pub fn gather(req: &ProbeRequest) -> HardwareFacts {
    let mut facts = HardwareFacts::default();
    for path in &req.paths {
        if let Some(path_facts) = stat(path) {
            facts
                .paths
                .insert(path.to_string_lossy().into_owned(), path_facts);
        }
    }
    #[cfg(unix)]
    {
        facts.groups = unix::groups(Path::new("/etc/group"));
        facts.service_user = unix::user(Path::new("/etc/passwd"), &req.service_user);
        facts.service_user_groups = unix::user_groups(Path::new("/etc/group"), &req.service_user);
    }
    #[cfg(target_os = "linux")]
    {
        facts.udev_rules = linux::udev_rules(
            &[
                Path::new("/etc/udev/rules.d"),
                Path::new("/run/udev/rules.d"),
                Path::new("/usr/lib/udev/rules.d"),
                Path::new("/lib/udev/rules.d"),
            ],
            &req.udev_rules,
        );
    }
    #[cfg(windows)]
    {
        record_com_ports(&mut facts, windows::com_ports());
    }
    // Staging replaces the USB scan rather than merging with it, so the
    // inventory cannot depend on what is plugged into the machine running
    // it. Only the inventory: everything gathered above still reads the
    // host.
    #[cfg(feature = "mock")]
    let scan = req
        .staged_usb
        .clone()
        .map_or_else(host_usb_scan, StagedUsbInventory::into_scan);
    #[cfg(not(feature = "mock"))]
    let scan = host_usb_scan();
    record_usb(&mut facts, scan);
    facts
}

/// The host's own USB inventory, from whichever collector this platform has.
fn host_usb_scan() -> Result<UsbScan, String> {
    #[cfg(target_os = "linux")]
    {
        linux::usb_inventory(Path::new("/sys/bus/usb/devices"))
    }
    #[cfg(target_os = "macos")]
    {
        macos::usb_inventory()
    }
    #[cfg(windows)]
    {
        windows::usb_inventory()
    }
    #[cfg(not(any(target_os = "linux", target_os = "macos", windows)))]
    {
        Ok(UsbScan::default())
    }
}

/// Land a collector's result on the facts, keeping "the scan failed"
/// distinct from "the bus is empty". On failure the inventory and the fault
/// list are left empty *and* marked unavailable, so a consumer that ignores
/// the marker gets no devices rather than a plausible-looking partial list.
fn record_usb(facts: &mut HardwareFacts, scan: Result<UsbScan, String>) {
    match scan {
        Ok(scan) => {
            for fault in &scan.faults {
                debug!(
                    record = %fault.record,
                    location = fault.location.as_deref().unwrap_or("unknown"),
                    reason = %fault.reason,
                    "USB record is not a working device; left out of the inventory"
                );
            }
            facts.usb = scan.devices;
            facts.usb_faults = scan.faults;
        }
        Err(reason) => {
            debug!(%reason, "USB inventory unavailable");
            facts.usb.clear();
            facts.usb_faults.clear();
            facts.usb_unavailable = Some(reason);
        }
    }
}

/// Land the COM-port listing on the facts, keeping "the listing failed"
/// distinct from "the host has no COM ports". On failure the list is left
/// empty *and* marked unavailable, as [`record_usb`] does for the bus, so a
/// consumer that ignores the marker gets no ports rather than a
/// plausible-looking list.
#[cfg(any(windows, test))]
fn record_com_ports(facts: &mut HardwareFacts, listing: Result<Vec<String>, String>) {
    match listing {
        Ok(ports) => facts.com_ports = ports,
        Err(reason) => {
            debug!(%reason, "COM-port listing unavailable");
            facts.com_ports.clear();
            facts.com_ports_unavailable = Some(reason);
        }
    }
}

/// `stat` one path, following symlinks (device paths are often udev
/// `by-id` links). `None` = absent or unreadable.
fn stat(path: &Path) -> Option<PathFacts> {
    let meta = match std::fs::metadata(path) {
        Ok(meta) => meta,
        Err(e) => {
            debug!(path = %path.display(), error = %e, "path probe: absent or unreadable");
            return None;
        }
    };
    let kind = kind_of(&meta);
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        Some(PathFacts {
            kind,
            mode: meta.mode() & 0o7777,
            uid: meta.uid(),
            gid: meta.gid(),
        })
    }
    #[cfg(not(unix))]
    {
        Some(PathFacts {
            kind,
            mode: 0,
            uid: 0,
            gid: 0,
        })
    }
}

fn kind_of(meta: &std::fs::Metadata) -> PathKind {
    #[cfg(unix)]
    {
        use std::os::unix::fs::FileTypeExt;
        if meta.file_type().is_char_device() {
            return PathKind::CharDevice;
        }
    }
    if meta.is_dir() {
        PathKind::Dir
    } else if meta.is_file() {
        PathKind::File
    } else {
        PathKind::Other
    }
}

#[cfg(unix)]
mod unix {
    use std::collections::BTreeMap;
    use std::path::Path;

    use tracing::debug;

    use super::UserFacts;

    /// Parse the whole group database in `/etc/group` format
    /// (`name:x:gid:members`) — all of it, not a requested subset: the
    /// checks resolve gids they could not have requested by name (a
    /// node's owning group set by the distro's udev defaults, a group
    /// named only inside an operator-edited rule). File parsing rather
    /// than `getgrnam` keeps the lookup testable against a staged file;
    /// hosts resolving groups purely through NSS plugins are out of its
    /// reach, and the checks' details say the judgment is a heuristic.
    pub fn groups(group_file: &Path) -> BTreeMap<String, u32> {
        let Ok(content) = std::fs::read_to_string(group_file) else {
            debug!(path = %group_file.display(), "group database unreadable");
            return BTreeMap::new();
        };
        content
            .lines()
            .filter_map(|line| {
                let mut fields = line.split(':');
                let name = fields.next()?;
                let _password = fields.next()?;
                let gid: u32 = fields.next()?.parse().ok()?;
                Some((name.to_string(), gid))
            })
            .collect()
    }

    /// The groups whose member list names the given user — the account's
    /// supplementary memberships in `/etc/group` format
    /// (`name:x:gid:member1,member2`). The primary group lives in the
    /// passwd entry, never here, so this list is exactly the supplementary
    /// set. Same heuristic reach as [`groups`]: NSS-only memberships are
    /// invisible.
    pub fn user_groups(group_file: &Path, user: &str) -> Vec<String> {
        let Ok(content) = std::fs::read_to_string(group_file) else {
            debug!(path = %group_file.display(), "group database unreadable");
            return Vec::new();
        };
        content
            .lines()
            .filter_map(|line| {
                let mut fields = line.split(':');
                let name = fields.next()?;
                let _password = fields.next()?;
                let _gid = fields.next()?;
                let members = fields.next()?;
                members
                    .split(',')
                    .any(|member| member == user)
                    .then(|| name.to_string())
            })
            .collect()
    }

    /// Look up one user in a `/etc/passwd`-format database
    /// (`name:x:uid:gid:...`).
    pub fn user(passwd_file: &Path, name: &str) -> Option<UserFacts> {
        let content = match std::fs::read_to_string(passwd_file) {
            Ok(content) => content,
            Err(e) => {
                debug!(path = %passwd_file.display(), error = %e, "user database unreadable");
                return None;
            }
        };
        content.lines().find_map(|line| {
            let mut fields = line.split(':');
            if fields.next()? != name {
                return None;
            }
            let _password = fields.next()?;
            let uid: u32 = fields.next()?.parse().ok()?;
            let gid: u32 = fields.next()?.parse().ok()?;
            Some(UserFacts { uid, gid })
        })
    }
}

#[cfg(target_os = "linux")]
mod linux {
    use std::collections::BTreeMap;
    use std::path::Path;

    use tracing::debug;

    use super::{UsbDevice, UsbFault, UsbScan};

    /// Walk sysfs USB devices. An entry with an `idVendor` is a **candidate
    /// device record**; interfaces have none and are skipped silently, as
    /// they always were (root hubs, `usb1`, do carry one and are listed like
    /// any device). A candidate that cannot be read in full is reported as a
    /// fault rather than a partial record — a device with no port is
    /// indistinguishable from one whose port did not match a claim — and
    /// the walk carries on. Only an unreadable directory fails the scan.
    ///
    /// The entry's own directory name *is* the port path: `1-4.2` reads as
    /// bus 1, root port 4, hub port 2. A device whose enumeration failed
    /// never gets an entry, so the kernel log is the only trace of one.
    pub fn usb_inventory(devices_dir: &Path) -> Result<UsbScan, String> {
        let entries = std::fs::read_dir(devices_dir).map_err(|e| {
            debug!(path = %devices_dir.display(), error = %e, "sysfs USB walk failed");
            format!("sysfs USB walk failed at {}: {e}", devices_dir.display())
        })?;
        let mut scan = UsbScan::default();
        for entry in entries {
            let entry = entry.map_err(|e| {
                debug!(error = %e, "sysfs USB walk could not read an entry");
                format!(
                    "sysfs USB walk could not read an entry under {}: {e}",
                    devices_dir.display()
                )
            })?;
            let dir = entry.path();
            let Some(vendor) = read_attr(&dir, "idVendor") else {
                continue;
            };
            let model = read_attr(&dir, "product");
            let fault =
                |product: Option<String>, location: Option<String>, reason: &str| UsbFault {
                    record: dir.display().to_string(),
                    vendor: Some(vendor.clone()),
                    product,
                    model: model.clone(),
                    location,
                    reason: reason.to_string(),
                };
            let Some(port) = dir.file_name().and_then(|name| name.to_str()) else {
                scan.faults.push(fault(
                    None,
                    None,
                    "its sysfs entry name is not valid UTF-8, so its port cannot be named",
                ));
                continue;
            };
            // `idProduct` is mandatory in the device descriptor, so a
            // candidate missing it is an unreadable entry rather than a
            // device without one — in practice, one unplugged mid-walk.
            // Defaulting it to empty would leave a plausible-looking record
            // that no VID:PID match can hit. `model` and `serial` are
            // genuinely optional and stay that way.
            let Some(product) = read_attr(&dir, "idProduct") else {
                scan.faults.push(fault(
                    None,
                    Some(port.to_string()),
                    "it names a vendor but no readable idProduct, which usually means it was \
                     unplugged during the scan",
                ));
                continue;
            };
            scan.devices.push(UsbDevice {
                vendor,
                product,
                model,
                port: Some(port.to_string()),
                serial: read_attr(&dir, "serial"),
            });
        }
        scan.devices.sort_by(|a, b| {
            (&a.vendor, &a.product, &a.port).cmp(&(&b.vendor, &b.product, &b.port))
        });
        scan.faults.sort_by(|a, b| a.record.cmp(&b.record));
        Ok(scan)
    }

    fn read_attr(dir: &Path, attr: &str) -> Option<String> {
        let content = std::fs::read_to_string(dir.join(attr)).ok()?;
        let trimmed = content.trim();
        (!trimmed.is_empty()).then(|| trimmed.to_string())
    }

    /// The effective installed copy of each expected rule file: the first
    /// hit in precedence order wins, exactly as udev shadows same-named
    /// files across its rules directories.
    pub fn udev_rules(dirs: &[&Path], names: &[String]) -> BTreeMap<String, String> {
        names
            .iter()
            .filter_map(|name| {
                dirs.iter().find_map(|dir| {
                    std::fs::read_to_string(dir.join(name))
                        .ok()
                        .map(|content| (name.clone(), content))
                })
            })
            .collect()
    }
}

/// Running a passive inventory query as a child process, under a deadline.
///
/// Extracted shape, not a general facility: see the tracking issue for
/// folding this and `plate-solver`'s `spawn_with_deadline` into one crate.
// `test` joins the platform gates so the deadline logic is compiled —
// and therefore linted and exercised — on every CI leg, not only the
// two that ship it. The same reason the `windows` parsers below are
// gated that way.
#[cfg(any(target_os = "macos", windows, test))]
mod bounded {
    use std::io::Read;
    use std::process::{Child, Command, Stdio};
    use std::sync::mpsc;
    use std::thread;
    use std::time::{Duration, Instant};

    /// A passive USB scan is a handful of cached reads; anything slower is
    /// a wedged child, not a slow one.
    pub const DEADLINE: Duration = Duration::from_secs(10);

    /// How long a child gets to exit after closing stdout, before it is
    /// killed. It has already produced its output at this point.
    pub(super) const REAP_GRACE: Duration = Duration::from_secs(1);

    /// Run `cmd` and return its stdout, or why it could not be trusted.
    ///
    /// The deadline is on **draining the child's output**, not on the
    /// child exiting, and that distinction is the whole point: a child
    /// that fills its stdout pipe buffer blocks writing and never exits,
    /// so waiting on exit would kill a healthy `system_profiler` on a
    /// machine with a populated bus. Reading continuously means the buffer
    /// never fills and EOF arrives when the child closes stdout.
    pub fn capture(cmd: &mut Command, deadline: Duration) -> Result<Vec<u8>, String> {
        let mut child = cmd
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .map_err(|e| format!("could not start {cmd:?}: {e}"))?;
        let mut stdout = child
            .stdout
            .take()
            .ok_or_else(|| format!("{cmd:?} produced no stdout pipe"))?;

        let (tx, rx) = mpsc::channel();
        thread::spawn(move || {
            let mut buffer = Vec::new();
            let outcome = stdout.read_to_end(&mut buffer).map(|_| buffer);
            // A closed receiver means the deadline already fired; the
            // child is being killed and nobody wants this any more.
            drop(tx.send(outcome));
        });

        match rx.recv_timeout(deadline) {
            Ok(Ok(bytes)) => match reap(&mut child) {
                Ok(status) if status.success() => Ok(bytes),
                Ok(status) => Err(format!("{cmd:?} exited with {status}")),
                Err(e) => Err(e),
            },
            Ok(Err(e)) => {
                kill(&mut child);
                Err(format!("{cmd:?} output could not be read: {e}"))
            }
            Err(_) => {
                kill(&mut child);
                Err(format!("{cmd:?} did not finish within {deadline:?}"))
            }
        }
    }

    /// Wait for a child that has already closed stdout, bounded — so that
    /// a process which lingers after producing its output cannot hang the
    /// gather either.
    pub(super) fn reap(child: &mut Child) -> Result<std::process::ExitStatus, String> {
        // Measured as elapsed time rather than a precomputed instant: adding
        // to an `Instant` can overflow, and there is no sensible answer for a
        // clock that cannot represent one second from now.
        let waiting_since = Instant::now();
        loop {
            match child.try_wait() {
                Ok(Some(status)) => return Ok(status),
                Ok(None) if waiting_since.elapsed() < REAP_GRACE => {
                    thread::sleep(Duration::from_millis(10));
                }
                Ok(None) => {
                    kill(child);
                    return Err("child produced output but did not exit".to_string());
                }
                Err(e) => return Err(format!("child could not be reaped: {e}")),
            }
        }
    }

    /// Kill and reap, so no child is orphaned. Both steps are best-effort:
    /// the caller is already reporting a failure and has nothing better to
    /// do with a second one.
    fn kill(child: &mut Child) {
        drop(child.kill());
        drop(child.wait());
    }
}

/// Gated on `test` as well as macOS so the pure tree walk — where the
/// device/fault split lives — is exercised by every platform's CI leg; the
/// `system_profiler` call stays macOS-only.
#[cfg(any(target_os = "macos", test))]
mod macos {
    #[cfg(target_os = "macos")]
    use std::process::Command;

    #[cfg(target_os = "macos")]
    use tracing::debug;

    use super::{UsbDevice, UsbFault, UsbScan};

    /// `system_profiler -json SPUSBDataType`. Only the query itself
    /// failing, or returning something other than its report, fails the
    /// scan; a device that cannot be placed is reported as a fault.
    #[cfg(target_os = "macos")]
    pub fn usb_inventory() -> Result<UsbScan, String> {
        let output = super::bounded::capture(
            Command::new("system_profiler").args(["-json", "SPUSBDataType"]),
            super::bounded::DEADLINE,
        )
        .map_err(|e| {
            debug!(error = %e, "system_profiler query failed");
            format!("macOS USB inventory failed: {e}")
        })?;
        let value = serde_json::from_slice::<serde_json::Value>(&output).map_err(|e| {
            debug!(error = %e, "system_profiler output is not valid JSON");
            format!("macOS USB inventory returned unparsable JSON: {e}")
        })?;
        parse_system_profiler(&value).map_err(|e| {
            debug!(error = %e, "system_profiler output is not a USB report");
            format!("macOS USB inventory returned an unparsable report: {e}")
        })
    }

    /// Split `system_profiler`'s tree into the working devices and the
    /// faults. Hubs nest their devices under `_items`, so the walk recurses.
    ///
    /// A report with no `SPUSBDataType` list is not an empty bus: it is a
    /// `system_profiler` that did not answer for that data type (reported
    /// for macOS 26, unverified), so it fails the scan rather than reading as one with
    /// no devices. An empty list is an empty bus.
    pub fn parse_system_profiler(value: &serde_json::Value) -> Result<UsbScan, String> {
        let top = value
            .get("SPUSBDataType")
            .and_then(|v| v.as_array())
            .ok_or_else(|| "it has no SPUSBDataType list".to_string())?;
        let mut scan = UsbScan::default();
        for item in top {
            walk(item, &mut scan);
        }
        Ok(scan)
    }

    fn walk(item: &serde_json::Value, scan: &mut UsbScan) {
        // A candidate device record is one presenting a vendor id; the
        // tree also carries controllers and other non-device nodes, which
        // are skipped silently as they always were.
        if let (Some(vendor), Some(product)) = (
            item.get("vendor_id").and_then(hex_field),
            item.get("product_id").and_then(hex_field),
        ) {
            let model = item.get("_name").and_then(text_field);
            match item.get("location_id").and_then(location_id) {
                Some(port) => scan.devices.push(UsbDevice {
                    vendor,
                    product,
                    model,
                    port: Some(port),
                    serial: item.get("serial_num").and_then(text_field),
                }),
                None => scan.faults.push(UsbFault {
                    record: model
                        .clone()
                        .unwrap_or_else(|| format!("{vendor}:{product}")),
                    vendor: Some(vendor),
                    product: Some(product),
                    model,
                    location: None,
                    reason: "it reports no usable location_id, so its port cannot be named"
                        .to_string(),
                }),
            }
        }
        if let Some(children) = item.get("_items").and_then(|v| v.as_array()) {
            for child in children {
                walk(child, scan);
            }
        }
    }

    /// `location_id` renders as `0x14200000` or `0x14200000 / 3`. Only the
    /// hex encodes the port chain; the trailing field is the USB device
    /// address, which is assigned in attach order and changes on replug —
    /// so keeping it would make the key unstable in exactly the situation
    /// the key exists to survive.
    fn location_id(value: &serde_json::Value) -> Option<String> {
        let text = value.as_str()?;
        let token = text.split_whitespace().next()?;
        let hex = token.strip_prefix("0x")?;
        (!hex.is_empty() && hex.chars().all(|c| c.is_ascii_hexdigit()))
            .then(|| token.to_lowercase())
    }

    /// `vendor_id` renders as `0x0403` or `0x0403  (Vendor Name)`.
    /// A descriptor string the device actually published. An empty one is
    /// not a shorter name, it is the absence of a name — which is what
    /// `None` means, and what the Linux and Windows collectors already
    /// return for the same case. `serial_num` was already read this way;
    /// `_name` was not, and that was the inconsistency.
    fn text_field(value: &serde_json::Value) -> Option<String> {
        let text = value.as_str()?.trim();
        (!text.is_empty()).then(|| text.to_string())
    }

    fn hex_field(value: &serde_json::Value) -> Option<String> {
        let text = value.as_str()?;
        let hex = text.strip_prefix("0x")?;
        let hex: String = hex.chars().take_while(char::is_ascii_hexdigit).collect();
        (hex.len() == 4).then(|| hex.to_lowercase())
    }
}

/// Gated on `test` as well as `windows` so the **pure parsers** below —
/// the location-path selection and the serial heuristic, which is where
/// the judgement lives, and the COM-port value filter — are exercised by
/// every platform's CI leg rather than only the Windows one. The impure
/// entry points stay Windows-only.
#[cfg(any(windows, test))]
mod windows {
    #[cfg(windows)]
    use std::process::Command;

    use tracing::debug;

    use super::{UsbDevice, UsbFault, UsbScan};

    #[cfg(windows)]
    fn powershell(script: &str) -> Result<String, String> {
        super::bounded::capture(
            Command::new("powershell.exe").args([
                "-NoProfile",
                "-NonInteractive",
                "-Command",
                script,
            ]),
            super::bounded::DEADLINE,
        )
        .map(|bytes| String::from_utf8_lossy(&bytes).into_owned())
        .map_err(|e| {
            debug!(error = %e, "powershell query failed");
            e
        })
    }

    /// The key every serial driver registers its ports under: one value
    /// per port, named after the kernel device (`\Device\Serial0`), whose
    /// string data is the port name (`COM3`). It is the key
    /// `[System.IO.Ports.SerialPort]::GetPortNames()` reads. `HKLM\HARDWARE`
    /// is volatile: Windows rebuilds it at every boot.
    const SERIALCOMM: &str = r"HARDWARE\DEVICEMAP\SERIALCOMM";

    /// `HRESULT_FROM_WIN32(ERROR_FILE_NOT_FOUND)`: the key does not exist.
    const KEY_NOT_FOUND: i32 = 0x8007_0002_u32.cast_signed();

    /// How many times the key is read before a listing that changes on
    /// every read is given up on.
    const READ_ATTEMPTS: usize = 3;

    /// The host's COM ports, read in-process from [`SERIALCOMM`] — no
    /// child process, so no start-up time, deadline or shell language mode
    /// can fail the listing. A key that exists but cannot be opened, whose
    /// values cannot be listed, or that never reads the same twice is a
    /// listing that failed, returned as such and never read as a host
    /// without ports.
    #[cfg(windows)]
    pub fn com_ports() -> Result<Vec<String>, String> {
        use windows_registry::LOCAL_MACHINE;

        let key = match LOCAL_MACHINE.open(SERIALCOMM) {
            Ok(key) => key,
            Err(e) => return listing_without_key(e.code().0, &e),
        };
        settled(|| {
            let values = key
                .values()
                .map_err(|e| listing_failed("list the values of", &e))?;
            Ok(port_names(
                values.map(|(device, value)| (device, port_text(&value))),
            ))
        })
    }

    /// A registry value's data as UTF-16 when the value is a string —
    /// `None` when it is not, which names no port. An expandable string is
    /// read unexpanded: a port name holds no environment variable.
    #[cfg(windows)]
    pub fn port_text(value: &windows_registry::Value) -> Option<Vec<u16>> {
        use windows_registry::Type;

        matches!(value.ty(), Type::String | Type::ExpandString).then(|| value.as_wide().to_vec())
    }

    /// The listing once two reads in a row agree.
    ///
    /// The key's values are enumerated by index, so a port that arrives or
    /// leaves during a read (a hot-plug race of microseconds) can shift the
    /// indices or end the enumeration early — a short list that still reads
    /// as a success, and can be missing a port that never changed. A read
    /// that a second read confirms was not torn. A key that changes on
    /// every one of [`READ_ATTEMPTS`] reads is a listing that failed, and a
    /// re-run of doctor reads it clean. (A debug build of the registry
    /// crate asserts on one shape of that race instead of stopping; the
    /// release build doctor ships stops.)
    pub fn settled(
        mut read: impl FnMut() -> Result<Vec<String>, String>,
    ) -> Result<Vec<String>, String> {
        let mut previous = read()?;
        for _ in 1..READ_ATTEMPTS {
            let current = read()?;
            if current == previous {
                return Ok(current);
            }
            debug!(
                ?previous,
                ?current,
                "serial-port registry key changed during the read"
            );
            previous = current;
        }
        Err(listing_failed(
            "read",
            &format!("its values changed on each of {READ_ATTEMPTS} reads"),
        ))
    }

    /// The listing when [`SERIALCOMM`] could not be opened, judged by the
    /// error's `HRESULT`: a key that does not exist is a host with no
    /// serial driver loaded since boot — no ports, not a failure — and any
    /// other error is a listing that could not be read.
    pub fn listing_without_key(
        code: i32,
        error: &impl std::fmt::Display,
    ) -> Result<Vec<String>, String> {
        if code == KEY_NOT_FOUND {
            debug!("no serial-port registry key: the host lists no COM ports");
            return Ok(Vec::new());
        }
        Err(listing_failed("open", error))
    }

    /// Why the listing failed: the step, the key, and Windows' own words.
    pub fn listing_failed(step: &str, error: &impl std::fmt::Display) -> String {
        format!("Windows COM-port listing failed: could not {step} HKLM\\{SERIALCOMM}: {error}")
    }

    /// The port names under [`SERIALCOMM`], from each value's name (the
    /// kernel device, kept for the debug trail) and its data as UTF-16 when
    /// the value is a string — `None` when it is not.
    ///
    /// A value that is not a string, or whose string is blank, names no
    /// port and is skipped: one odd driver registration must not fail the
    /// listing for every other port, as a non-string value fails
    /// `GetPortNames()`. A name
    /// ends at its first NUL — a driver that writes a terminated name into
    /// a longer buffer leaves junk after it, which would never match a
    /// configured port. No name is otherwise validated: one a driver
    /// registered oddly (com0com's `CNCA0`) stays in the list, where
    /// `hardware.serial-node` shows it.
    pub fn port_names(values: impl IntoIterator<Item = (String, Option<Vec<u16>>)>) -> Vec<String> {
        values
            .into_iter()
            .filter_map(|(device, text)| {
                let Some(text) = text else {
                    debug!(%device, "serial-port registry value is not a string; skipped");
                    return None;
                };
                let name = text.split(|&unit| unit == 0).next().unwrap_or_default();
                let name = String::from_utf16_lossy(name);
                let name = name.trim();
                if name.is_empty() {
                    debug!(%device, "serial-port registry value is blank; skipped");
                    return None;
                }
                Some(name.to_string())
            })
            .collect()
    }

    /// Present USB devices from `PnP`: the instance id carries
    /// `USB\VID_xxxx&PID_xxxx\...`; the bus-reported device description is
    /// the product string the device itself sent; the location path is the
    /// port chain; the problem code (`ConfigManagerErrorCode`, read off the
    /// object `Get-PnpDevice` already returned) says whether Windows has
    /// the device working, and the friendly name is what Windows lists it
    /// as — for its enumeration-failure placeholder, what went wrong.
    ///
    /// A null problem code is printed as an empty field rather than cast:
    /// `[uint32]$null` is `0`, which would read as a working device.
    ///
    /// The description and the friendly name are text a device (or its
    /// driver) supplies, so their control characters become spaces before
    /// they are printed: a tab or a line break in a product string must
    /// not add a field or a line, or one odd device would fail the whole
    /// listing (`parse_pnp_listing`). The instance id, the location paths
    /// and the problem code are Windows' own and need no cleaning. In this
    /// Rust literal the regex's backslash is doubled; a single `\x00`-style
    /// escape would put a real NUL into the argument, which `Command`
    /// refuses.
    pub(super) const USB_QUERY: &str =
        "Get-PnpDevice -PresentOnly -ErrorAction SilentlyContinue | \
         Where-Object { $_.InstanceId -like 'USB\\VID_*' } | \
         ForEach-Object { \
             $desc = [string](Get-PnpDeviceProperty -InstanceId $_.InstanceId \
                 -KeyName DEVPKEY_Device_BusReportedDeviceDesc \
                 -ErrorAction SilentlyContinue).Data -replace '\\p{Cc}', ' '; \
             $paths = (Get-PnpDeviceProperty -InstanceId $_.InstanceId \
                 -KeyName DEVPKEY_Device_LocationPaths \
                 -ErrorAction SilentlyContinue).Data; \
             $code = if ($null -ne $_.ConfigManagerErrorCode) \
                 { [uint32]$_.ConfigManagerErrorCode }; \
             $name = [string]$_.FriendlyName -replace '\\p{Cc}', ' '; \
             \"$($_.InstanceId)`t$desc`t$($paths -join '|')`t$code`t$name\" }";

    #[cfg(windows)]
    pub fn usb_inventory() -> Result<UsbScan, String> {
        let listing =
            powershell(USB_QUERY).map_err(|e| format!("Windows USB inventory failed: {e}"))?;
        parse_pnp_listing(&listing).map_err(|e| {
            debug!(error = %e, "PnP listing is not the query's output");
            format!("Windows USB inventory returned an unparsable listing: {e}")
        })
    }

    /// Split the collector's listing into the working devices and the
    /// faults. One line per `PnP` instance, tab-separated: instance id,
    /// bus-reported description, location paths joined by `|`, problem
    /// code, friendly name.
    ///
    /// The query keeps only `USB\VID_…` instances — root hubs and other
    /// non-device instances never reach this parser — and prints all five
    /// fields for each, blank or not, with the control characters in the
    /// device-supplied text replaced (`USB_QUERY`), so no device's strings
    /// can add a field or a line. A composite device's per-interface child
    /// (`USB\VID_…&PID_…&MI_nn\…`), a function of a device already listed
    /// under its own record, is skipped. A record that is not a working
    /// device — Windows reports a problem code or none, or its ids or its
    /// `PCIROOT(` port cannot be read — is a fault, and no such record can
    /// fail the listing.
    ///
    /// A line the query cannot have produced does: one that is not a
    /// `USB\VID_…` record or has fewer than the five fields, and, for a
    /// record the parser reads (not an interface child, ids readable), a
    /// problem code that is neither blank nor a number. That is output the
    /// parser does not understand, not a device it understood to be broken,
    /// and reading it as an empty or thinner bus would turn a collector
    /// failure into absence and cable diagnoses.
    pub fn parse_pnp_listing(listing: &str) -> Result<UsbScan, String> {
        let mut scan = UsbScan::default();
        for (line, number) in listing.lines().zip(1_usize..) {
            let line = line.trim_end_matches('\r');
            if line.trim().is_empty() {
                continue;
            }
            let fields: Vec<&str> = line.splitn(5, '\t').collect();
            // The query's `-like 'USB\VID_*'` is case-insensitive.
            let Some(rest) = fields.first().and_then(|instance| vid_rest(instance)) else {
                return Err(format!(
                    "line {number} is not a USB\\VID_ record: {}",
                    excerpt(line)
                ));
            };
            let [instance, desc, paths, problem, friendly] = fields[..] else {
                return Err(format!(
                    "line {number} has {} of the 5 tab-separated fields: {}",
                    fields.len(),
                    excerpt(line)
                ));
            };
            let (desc, problem, friendly) = (desc.trim(), problem.trim(), friendly.trim());
            // The Windows counterpart of a Linux interface entry: the
            // composite parent carries the device's ids and port.
            if rest
                .split('\\')
                .next()
                .is_some_and(|ids| ids.to_ascii_uppercase().contains("&MI_"))
            {
                continue;
            }
            let ids = usb_ids(rest);
            let model = (!desc.is_empty()).then(|| desc.to_string());
            let location = location_path(paths).or_else(|| first_path(paths));
            let fault = |reason: String| UsbFault {
                record: instance.to_string(),
                vendor: ids.as_ref().map(|(vendor, _)| vendor.clone()),
                product: ids.as_ref().map(|(_, product)| product.clone()),
                model: model.clone(),
                location: location.clone(),
                reason,
            };
            let Some((vendor, product)) = ids.clone() else {
                scan.faults.push(fault(
                    "its instance id does not name a vendor and product".to_string(),
                ));
                continue;
            };
            if problem.is_empty() {
                scan.faults.push(fault(
                    "Windows did not say whether it is working (no problem code)".to_string(),
                ));
                continue;
            }
            match problem.parse::<u32>() {
                Ok(0) => {}
                Ok(code) => {
                    scan.faults.push(fault(not_working(code, friendly)));
                    continue;
                }
                Err(_) => {
                    return Err(format!(
                        "line {number} has a problem code that is not a number ({problem:?})"
                    ));
                }
            }
            let Some(port) = location_path(paths) else {
                let reason = if paths.trim().is_empty() {
                    "its location paths could not be read, so its port cannot be named"
                } else {
                    "it reports no PCIROOT location path, so its port cannot be named"
                };
                scan.faults.push(fault(reason.to_string()));
                continue;
            };
            scan.devices.push(UsbDevice {
                vendor,
                product,
                model,
                port: Some(port),
                serial: instance_serial(instance),
            });
        }
        Ok(scan)
    }

    /// What follows `USB\VID_` in an instance id, matched without regard to
    /// case as the query's `-like` matches it; `None` for any other id.
    fn vid_rest(instance: &str) -> Option<&str> {
        const PREFIX: &str = "USB\\VID_";
        instance
            .get(..PREFIX.len())
            .filter(|start| start.eq_ignore_ascii_case(PREFIX))
            .and_then(|_| instance.get(PREFIX.len()..))
    }

    /// At most the first 120 characters of a line, quoted, for an error
    /// message: a line the query did not produce could be anything, of any
    /// length.
    fn excerpt(line: &str) -> String {
        let mut short: String = line.chars().take(120).collect();
        if short.len() < line.len() {
            short.push('…');
        }
        format!("{short:?}")
    }

    /// The two ids after `USB\VID_`, lowercased — `None` unless the
    /// instance id spells both as four hex digits.
    fn usb_ids(rest: &str) -> Option<(String, String)> {
        let vendor = rest.get(..4)?.to_lowercase();
        if !rest.get(4..9)?.eq_ignore_ascii_case("&PID_") {
            return None;
        }
        let product = rest.get(9..13)?.to_lowercase();
        (super::is_usb_id(&vendor) && super::is_usb_id(&product)).then_some((vendor, product))
    }

    /// Why Windows has the device down: the problem code with Device
    /// Manager's meaning for it, and what Windows lists the device as —
    /// which, for the enumeration-failure placeholder, is itself the
    /// diagnosis (*Unknown USB Device (Device Descriptor Request Failed)*).
    fn not_working(code: u32, friendly: &str) -> String {
        let problem = problem_meaning(code).map_or_else(
            || format!("problem code {code}"),
            |meaning| format!("problem code {code}: {meaning}"),
        );
        if friendly.is_empty() {
            format!("Windows reports it not working ({problem})")
        } else {
            format!("Windows reports it not working ({problem}); Windows lists it as {friendly:?}")
        }
    }

    /// Device Manager's meaning for the problem codes a USB device
    /// realistically shows (`CM_PROB_*`); any other code is reported by
    /// number alone.
    const fn problem_meaning(code: u32) -> Option<&'static str> {
        Some(match code {
            1 => "it is not configured correctly",
            10 => "it cannot start",
            14 => "it cannot work properly until the computer restarts",
            18 => "its drivers need reinstalling",
            22 => "it is disabled",
            24 => "it is not present, not working properly, or missing drivers",
            28 => "its drivers are not installed",
            31 => "Windows cannot load the drivers it requires",
            39 => "Windows cannot load its driver, which may be corrupted or missing",
            43 => "Windows stopped it because it reported problems",
            52 => "Windows cannot verify the digital signature of its drivers",
            _ => return None,
        })
    }

    /// Any location path at all, for a record without a `PCIROOT(` one —
    /// not a port, but enough for an operator to find the socket (the ACPI
    /// chain ends in the root-hub port name, `ACPI(HS05)`).
    fn first_path(paths: &str) -> Option<String> {
        paths
            .split('|')
            .map(str::trim)
            .find(|path| !path.is_empty())
            .map(str::to_string)
    }

    /// `DEVPKEY_Device_LocationPaths` is **multi-valued**: a device
    /// typically publishes both a `PCIROOT(…)`-rooted chain and an
    /// `ACPI(…)` one, and a device whose descriptor request failed may
    /// publish only the ACPI form. Select the `PCIROOT` spelling rather
    /// than taking the first element, whose kind is not guaranteed.
    fn location_path(paths: &str) -> Option<String> {
        paths
            .split('|')
            .map(str::trim)
            .find(|path| path.starts_with("PCIROOT("))
            .map(str::to_string)
    }

    /// The instance id's third segment is the device's serial **only when
    /// the device published one**. Where it did not, Windows synthesizes a
    /// parent-relative id containing `&` separators (`6&4213695&0&3`),
    /// which encodes the port rather than the device and would be a
    /// different string for the same camera in another socket.
    fn instance_serial(instance: &str) -> Option<String> {
        let segment = instance.rsplit('\\').next()?.trim();
        (!segment.is_empty() && !segment.contains('&')).then(|| segment.to_string())
    }
}

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
mod tests {
    use super::*;

    #[test]
    fn test_facts_parse_permissively_with_absent_sections() {
        let facts: HardwareFacts = serde_json::from_str(r#"{ "future_field": 1 }"#).unwrap();
        assert!(facts.paths.is_empty());
        assert_eq!(facts.usb, Vec::<UsbDevice>::new());
        assert!(facts.service_user.is_none());
        assert_eq!(facts.com_ports_unavailable, None);
    }

    #[test]
    fn test_usb_match_requires_every_declared_field() {
        let facts: HardwareFacts = serde_json::from_str(
            r#"{ "usb": [
                { "vendor": "0403", "product": "6015", "model": "Falcon Rotator" },
                { "vendor": "1618", "product": "c179" }
            ] }"#,
        )
        .unwrap();
        assert_eq!(facts.usb_present("0403", None, None), Some(true));
        assert_eq!(
            facts.usb_present("0403", Some("6015"), Some("Falcon")),
            Some(true)
        );
        assert_eq!(
            facts.usb_present("0403", Some("6015"), Some("PPBA")),
            Some(false),
            "the model substring must discriminate devices sharing a bridge VID:PID"
        );
        assert_eq!(facts.usb_present("0403", Some("6001"), None), Some(false));
        assert_eq!(
            facts.usb_present("1618", None, Some("Q-Focuser")),
            Some(false),
            "a declared model never matches a device that reports none"
        );
        assert_eq!(facts.usb_present("03c3", None, None), Some(false));
    }

    #[test]
    fn test_unavailable_inventory_answers_nothing_rather_than_absent() {
        let facts: HardwareFacts =
            serde_json::from_str(r#"{ "usb": [], "usb_unavailable": "sysfs USB walk failed" }"#)
                .unwrap();
        assert_eq!(
            facts.usb_present("0403", None, None),
            None,
            "a failed scan must not read as an absent device"
        );
    }

    #[test]
    fn test_absent_unavailable_marker_means_the_scan_succeeded() {
        let facts: HardwareFacts = serde_json::from_str(r#"{ "usb": [] }"#).unwrap();
        assert_eq!(
            facts.usb_present("0403", None, None),
            Some(false),
            "a fixture written before the marker existed still means an empty bus"
        );
    }

    /// A COM-port listing that failed, as the collector reports one.
    const COM_LISTING_DENIED: &str = r"Windows COM-port listing failed: could not open HKLM\HARDWARE\DEVICEMAP\SERIALCOMM: Access is denied. (0x80070005)";

    #[test]
    fn test_unavailable_com_port_listing_answers_nothing_rather_than_absent() {
        let facts: HardwareFacts = serde_json::from_value(serde_json::json!({
            "com_ports": [],
            "com_ports_unavailable": COM_LISTING_DENIED,
        }))
        .unwrap();
        assert_eq!(
            facts.com_port_present("COM4"),
            None,
            "a failed listing must not read as an absent port"
        );
    }

    /// The marker wins over a list that contradicts it: a staged file can
    /// say both, and the answer is "unknown", never "present".
    #[test]
    fn test_unavailable_marker_outranks_a_listed_com_port() {
        let facts: HardwareFacts = serde_json::from_value(serde_json::json!({
            "com_ports": ["COM4"],
            "com_ports_unavailable": COM_LISTING_DENIED,
        }))
        .unwrap();
        assert_eq!(facts.com_port_present("COM4"), None);
    }

    #[test]
    fn test_absent_com_port_marker_means_the_listing_succeeded() {
        let facts: HardwareFacts = serde_json::from_str(r#"{ "com_ports": [] }"#).unwrap();
        assert_eq!(
            facts.com_port_present("COM4"),
            Some(false),
            "a fixture written before the marker existed still means no ports"
        );
    }

    /// Windows compares device names without regard to case.
    #[test]
    fn test_com_port_match_ignores_ascii_case() {
        let facts: HardwareFacts = serde_json::from_str(r#"{ "com_ports": ["com4"] }"#).unwrap();
        assert_eq!(facts.com_port_present("COM4"), Some(true));
    }

    /// A port name is matched whole: neither one that extends a listed name
    /// nor one that a listed name extends is present.
    #[test]
    fn test_com_port_match_is_whole_not_a_prefix() {
        let listed_short: HardwareFacts =
            serde_json::from_str(r#"{ "com_ports": ["COM1"] }"#).unwrap();
        let listed_long: HardwareFacts =
            serde_json::from_str(r#"{ "com_ports": ["COM14"] }"#).unwrap();
        assert_eq!(listed_short.com_port_present("COM14"), Some(false));
        assert_eq!(listed_long.com_port_present("COM1"), Some(false));
    }

    /// A failed listing has no opinion about the ports — none survive it.
    #[test]
    fn test_a_failed_com_port_listing_leaves_no_ports() {
        let mut facts: HardwareFacts =
            serde_json::from_str(r#"{ "com_ports": ["COM3"] }"#).unwrap();
        record_com_ports(&mut facts, Err(COM_LISTING_DENIED.to_string()));
        assert_eq!(facts.com_ports, Vec::<String>::new());
        assert_eq!(
            facts.com_ports_unavailable.as_deref(),
            Some(COM_LISTING_DENIED)
        );
    }

    /// A listing that ran lands its ports, an empty one included.
    #[test]
    fn test_a_com_port_listing_lands_its_ports_on_the_facts() {
        let mut facts = HardwareFacts::default();
        record_com_ports(&mut facts, Ok(vec!["COM3".to_string(), "COM4".to_string()]));
        assert_eq!(facts.com_ports, ["COM3", "COM4"]);
        assert_eq!(facts.com_ports_unavailable, None);
    }

    /// A dead device is not a present one: presence reads only the
    /// inventory, and the fault is found by the same identity rules.
    #[test]
    fn test_a_fault_is_matched_by_identity_but_never_counts_as_present() {
        let facts: HardwareFacts = serde_json::from_str(
            r#"{ "usb": [], "usb_faults": [
                { "record": "USB\\VID_2E8A&PID_000A\\E463B0531F4C3831",
                  "vendor": "2e8a", "product": "000a", "model": "Deep Sky Dad FP2",
                  "reason": "Windows reports it not working (problem code 10)" }
            ] }"#,
        )
        .unwrap();
        assert_eq!(
            facts.usb_present("2e8a", Some("000a"), Some("FP2")),
            Some(false)
        );
        let fault = facts
            .usb_fault_matching("2e8a", Some("000a"), Some("FP2"))
            .unwrap();
        assert_eq!(
            fault.reason,
            "Windows reports it not working (problem code 10)"
        );
        assert!(
            facts
                .usb_fault_matching("2e8a", Some("000a"), Some("PPBA"))
                .is_none(),
            "the model substring discriminates faults too"
        );
        assert!(facts.usb_fault_matching("0403", None, None).is_none());
    }

    /// A fault that could not read an id never matches an identity that
    /// declares one.
    #[test]
    fn test_a_fault_without_ids_matches_no_identity() {
        let facts: HardwareFacts = serde_json::from_str(
            r#"{ "usb": [], "usb_faults": [ { "record": "USB\\VID_ZZ", "reason": "unreadable" } ] }"#,
        )
        .unwrap();
        assert!(facts.usb_fault_matching("2e8a", None, None).is_none());
    }

    /// Same vendor, another product: some other device's fault, not the
    /// declared one's — and a fault whose product could not be read never
    /// matches an identity that declares one.
    #[test]
    fn test_a_fault_must_match_a_declared_product() {
        let facts: HardwareFacts = serde_json::from_str(
            r#"{ "usb": [], "usb_faults": [
                { "record": "USB\\VID_0483&PID_DF11\\1", "vendor": "0483", "product": "df11",
                  "reason": "problem code 43" },
                { "record": "1-9", "vendor": "0483", "reason": "no readable idProduct" }
            ] }"#,
        )
        .unwrap();
        assert_eq!(facts.usb_fault_matching("0483", Some("5740"), None), None);
        assert_eq!(
            facts
                .usb_fault_matching("0483", None, None)
                .map(|f| f.record.as_str()),
            Some("USB\\VID_0483&PID_DF11\\1"),
            "a vendor-only identity matches the first fault from that vendor"
        );
    }

    /// A failed scan has no opinion about the bus — neither devices nor
    /// faults survive it.
    #[test]
    fn test_a_failed_scan_leaves_no_devices_and_no_faults() {
        let mut facts: HardwareFacts = serde_json::from_str(
            r#"{ "usb": [ { "vendor": "0403", "product": "6015", "port": "1-1" } ],
                 "usb_faults": [ { "record": "1-9", "reason": "unplugged mid-scan" } ] }"#,
        )
        .unwrap();
        record_usb(
            &mut facts,
            Err("powershell.exe did not finish within 10s".to_string()),
        );
        assert_eq!(facts.usb, Vec::<UsbDevice>::new());
        assert_eq!(facts.usb_faults, Vec::<UsbFault>::new());
        assert_eq!(
            facts.usb_unavailable.as_deref(),
            Some("powershell.exe did not finish within 10s")
        );
    }

    /// A successful scan lands its devices and its faults side by side.
    #[test]
    fn test_a_scan_lands_devices_and_faults_on_the_facts() {
        let mut facts = HardwareFacts::default();
        record_usb(
            &mut facts,
            Ok(UsbScan {
                devices: vec![UsbDevice {
                    vendor: "0403".to_string(),
                    product: "6015".to_string(),
                    model: None,
                    port: Some("1-1".to_string()),
                    serial: None,
                }],
                faults: vec![UsbFault {
                    record: "1-9".to_string(),
                    vendor: Some("03c3".to_string()),
                    product: None,
                    model: None,
                    location: Some("1-9".to_string()),
                    reason: "unplugged mid-scan".to_string(),
                }],
            }),
        );
        assert_eq!(facts.usb.len(), 1);
        assert_eq!(facts.usb_faults.len(), 1);
        assert!(facts.usb_unavailable.is_none());
    }

    #[test]
    fn test_group_name_reverse_lookup() {
        let facts: HardwareFacts =
            serde_json::from_str(r#"{ "groups": { "dialout": 20, "plugdev": 46 } }"#).unwrap();
        assert_eq!(facts.group_name(46), Some("plugdev"));
        assert_eq!(facts.group_name(99), None);
    }

    #[test]
    fn test_stat_classifies_files_dirs_and_absence() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("f");
        std::fs::write(&file, "x").unwrap();
        assert_eq!(stat(&file).unwrap().kind, PathKind::File);
        assert_eq!(stat(dir.path()).unwrap().kind, PathKind::Dir);
        assert!(stat(&dir.path().join("absent")).is_none());
    }

    #[cfg(unix)]
    #[test]
    fn test_stat_reports_unix_ownership_and_mode() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("f");
        std::fs::write(&file, "x").unwrap();
        std::fs::set_permissions(&file, std::fs::Permissions::from_mode(0o640)).unwrap();
        let facts = stat(&file).unwrap();
        assert_eq!(facts.mode, 0o640);
        // The test process owns what it creates.
        assert_ne!(facts.mode & 0o600, 0);
    }

    #[cfg(unix)]
    #[test]
    fn test_group_and_user_database_parsing() {
        let dir = tempfile::tempdir().unwrap();
        let group = dir.path().join("group");
        std::fs::write(
            &group,
            "root:x:0:\ndialout:x:20:igor\nplugdev:x:46:\nmalformed line\n",
        )
        .unwrap();
        let groups = unix::groups(&group);
        assert_eq!(groups.get("dialout"), Some(&20));
        assert_eq!(groups.get("plugdev"), Some(&46));
        assert_eq!(
            groups.get("root"),
            Some(&0),
            "the whole database is gathered — checks resolve gids they \
             could not have requested by name"
        );
        assert!(!groups.contains_key("ghost"), "absent group stays absent");
        assert_eq!(groups.len(), 3, "the malformed line is skipped");

        let members = dir.path().join("group-members");
        std::fs::write(
            &members,
            "root:x:0:\n\
             dialout:x:20:igor\n\
             plugdev:x:46:igor,rusty-photon\n\
             video:x:44:rusty-photon-two\n\
             malformed line\n",
        )
        .unwrap();
        assert_eq!(
            unix::user_groups(&members, "rusty-photon"),
            vec!["plugdev".to_string()],
            "membership is exact — a member name merely containing the \
             user does not count, and empty member lists never match"
        );
        assert!(
            unix::user_groups(&members, "ghost").is_empty(),
            "a user in no member list has no supplementary groups"
        );
        assert_eq!(
            unix::user_groups(&dir.path().join("absent"), "rusty-photon"),
            Vec::<String>::new()
        );

        let passwd = dir.path().join("passwd");
        std::fs::write(
            &passwd,
            "root:x:0:0:root:/root:/bin/bash\n\
             rusty-photon:x:990:990::/var/lib/rusty-photon:/sbin/nologin\n",
        )
        .unwrap();
        let user = unix::user(&passwd, "rusty-photon").unwrap();
        assert_eq!((user.uid, user.gid), (990, 990));
        assert!(unix::user(&passwd, "ghost").is_none());
        assert!(unix::user(&dir.path().join("absent"), "rusty-photon").is_none());
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn test_sysfs_walk_and_udev_precedence() {
        let dir = tempfile::tempdir().unwrap();
        let devices = dir.path().join("devices");
        for (entry, vendor, product, model, serial) in [
            (
                "1-1",
                Some("0403"),
                Some("6015"),
                Some("Falcon Rotator"),
                Some("FT1ABCDE"),
            ),
            ("1-4.2", Some("1618"), Some("c179"), None, None),
            ("1-1:1.0", None, None, None, None), // an interface — no idVendor
        ] {
            let d = devices.join(entry);
            std::fs::create_dir_all(&d).unwrap();
            if let Some(v) = vendor {
                std::fs::write(d.join("idVendor"), format!("{v}\n")).unwrap();
            }
            if let Some(p) = product {
                std::fs::write(d.join("idProduct"), format!("{p}\n")).unwrap();
            }
            if let Some(m) = model {
                std::fs::write(d.join("product"), format!("{m}\n")).unwrap();
            }
            if let Some(sn) = serial {
                std::fs::write(d.join("serial"), format!("{sn}\n")).unwrap();
            }
        }
        let scan = linux::usb_inventory(&devices).unwrap();
        assert_eq!(scan.faults, Vec::<UsbFault>::new());
        let inventory = scan.devices;
        assert_eq!(inventory.len(), 2, "interfaces are not devices");
        assert_eq!(inventory[0].vendor, "0403");
        assert_eq!(inventory[0].model.as_deref(), Some("Falcon Rotator"));
        assert_eq!(
            inventory[0].port.as_deref(),
            Some("1-1"),
            "the sysfs directory name is the port path"
        );
        assert_eq!(inventory[0].serial.as_deref(), Some("FT1ABCDE"));
        assert_eq!(inventory[1].vendor, "1618");
        assert_eq!(inventory[1].model, None);
        assert_eq!(
            inventory[1].port.as_deref(),
            Some("1-4.2"),
            "a nested port chain survives verbatim"
        );
        assert_eq!(
            inventory[1].serial, None,
            "a device publishing no serial is normal, not a failure"
        );

        let etc = dir.path().join("etc-rules");
        let lib = dir.path().join("lib-rules");
        std::fs::create_dir_all(&etc).unwrap();
        std::fs::create_dir_all(&lib).unwrap();
        std::fs::write(lib.join("90-a.rules"), "packaged").unwrap();
        std::fs::write(etc.join("90-a.rules"), "override").unwrap();
        std::fs::write(lib.join("90-b.rules"), "only-lib").unwrap();
        let rules = linux::udev_rules(
            &[&etc, &lib],
            &[
                "90-a.rules".to_string(),
                "90-b.rules".to_string(),
                "90-c.rules".to_string(),
            ],
        );
        assert_eq!(
            rules.get("90-a.rules").map(String::as_str),
            Some("override"),
            "/etc shadows the packaged copy, same as udev"
        );
        assert_eq!(
            rules.get("90-b.rules").map(String::as_str),
            Some("only-lib")
        );
        assert!(!rules.contains_key("90-c.rules"));
    }

    /// A candidate that names a vendor but whose product cannot be read —
    /// a device unplugged mid-walk — is a fault: it neither becomes a
    /// record with an empty product that no VID:PID match could hit, nor
    /// costs the answer for the device beside it.
    #[cfg(target_os = "linux")]
    #[test]
    fn test_sysfs_entry_without_idproduct_is_a_fault_not_a_failed_scan() {
        let dir = tempfile::tempdir().unwrap();
        let devices = dir.path().join("devices");
        let healthy = devices.join("1-1");
        std::fs::create_dir_all(&healthy).unwrap();
        std::fs::write(healthy.join("idVendor"), "0403\n").unwrap();
        std::fs::write(healthy.join("idProduct"), "6015\n").unwrap();
        let half_read = devices.join("1-9");
        std::fs::create_dir_all(&half_read).unwrap();
        std::fs::write(half_read.join("idVendor"), "03c3\n").unwrap();
        std::fs::write(half_read.join("product"), "ASI662MC\n").unwrap();

        let scan = linux::usb_inventory(&devices).unwrap();

        assert_eq!(scan.devices.len(), 1, "the healthy device is still listed");
        assert_eq!(scan.devices[0].vendor, "0403");
        assert_eq!(
            scan.faults,
            vec![UsbFault {
                record: half_read.display().to_string(),
                vendor: Some("03c3".to_string()),
                product: None,
                model: Some("ASI662MC".to_string()),
                location: Some("1-9".to_string()),
                reason: "it names a vendor but no readable idProduct, which usually means it \
                         was unplugged during the scan"
                    .to_string(),
            }]
        );
    }

    /// Only an unreadable devices directory fails the Linux scan.
    #[cfg(target_os = "linux")]
    #[test]
    fn test_unreadable_sysfs_directory_fails_the_scan() {
        let dir = tempfile::tempdir().unwrap();
        let error = linux::usb_inventory(&dir.path().join("absent"))
            .expect_err("a directory that cannot be read is a failed scan");
        assert!(error.contains("sysfs USB walk failed"), "{error}");
    }

    /// An entry whose name is not UTF-8 has no port that can be named: a
    /// fault with no location, and the device beside it is still listed.
    #[cfg(target_os = "linux")]
    #[test]
    fn test_sysfs_entry_with_a_non_utf8_name_is_a_fault_without_a_location() {
        use std::os::unix::ffi::OsStrExt;

        let dir = tempfile::tempdir().unwrap();
        let devices = dir.path().join("devices");
        let healthy = devices.join("1-1");
        std::fs::create_dir_all(&healthy).unwrap();
        std::fs::write(healthy.join("idVendor"), "0403\n").unwrap();
        std::fs::write(healthy.join("idProduct"), "6015\n").unwrap();
        let odd = devices.join(std::ffi::OsStr::from_bytes(b"1-\xff"));
        std::fs::create_dir_all(&odd).unwrap();
        std::fs::write(odd.join("idVendor"), "03c3\n").unwrap();

        let scan = linux::usb_inventory(&devices).unwrap();

        assert_eq!(scan.devices.len(), 1);
        assert_eq!(scan.faults.len(), 1);
        assert_eq!(scan.faults[0].vendor.as_deref(), Some("03c3"));
        assert_eq!(scan.faults[0].location, None);
        assert_eq!(
            scan.faults[0].reason,
            "its sysfs entry name is not valid UTF-8, so its port cannot be named"
        );
    }

    /// Lines as the collector's PowerShell emits them: instance id, the
    /// bus-reported description, the joined location paths, the problem
    /// code and the friendly name. The values are real observations from
    /// the Starfront Windows rig, read on 2026-09-28 with every device
    /// powered.
    const RIG2_LISTING: &str = "\
USB\\VID_0403&PID_6001\\OP2CGIIA\tOptec USB/Serial Cable\tPCIROOT(0)#PCI(1400)#USBROOT(0)#USB(2)#USB(5)|ACPI(_SB_)#ACPI(PC00)#ACPI(XHCI)#ACPI(RHUB)#ACPI(HS02)#USB(5)\t0\tUSB Serial Converter
USB\\VID_0424&PID_2807\\5&27E528BF&0&2\tUSB2807 Hub\tPCIROOT(0)#PCI(1400)#USBROOT(0)#USB(2)|ACPI(_SB_)#ACPI(PC00)#ACPI(XHCI)#ACPI(RHUB)#ACPI(HS02)\t0\tGeneric USB Hub
USB\\VID_2E8A&PID_000A\\E463B0531F4C3831\tDeep Sky Dad FP2\tPCIROOT(0)#PCI(1400)#USBROOT(0)#USB(1)|ACPI(_SB_)#ACPI(PC00)#ACPI(XHCI)#ACPI(RHUB)#ACPI(HS01)\t0\tUSB Serial Device (COM4)
USB\\VID_0000&PID_0002\\5&27E528BF&0&5\t\tACPI(_SB_)#ACPI(PC00)#ACPI(XHCI)#ACPI(RHUB)#ACPI(HS05)\t43\tUnknown USB Device (Device Descriptor Request Failed)
USB\\VID_03C3&PID_662B\\6&21D0E52E&0&4\tASI662MC\tPCIROOT(0)#PCI(1400)#USBROOT(0)#USB(2)#USB(4)|ACPI(_SB_)#ACPI(PC00)#ACPI(XHCI)#ACPI(RHUB)#ACPI(HS02)#USB(4)\t0\tZWO ASI662MC Camera
USB\\VID_8087&PID_0033\\5&27E528BF&0&10\t\tPCIROOT(0)#PCI(1400)#USBROOT(0)#USB(10)|ACPI(_SB_)#ACPI(PC00)#ACPI(XHCI)#ACPI(RHUB)#ACPI(HS10)\t0\tIntel(R) Wireless Bluetooth(R)
USB\\VID_1618&PID_0679\\6&4213695&0&3\tQHY678U3G20-20230106\tPCIROOT(0)#PCI(1400)#USBROOT(0)#USB(14)#USB(3)|ACPI(_SB_)#ACPI(PC00)#ACPI(XHCI)#ACPI(RHUB)#ACPI(SS02)#USB(3)\t0\tQHY5IIISeries_IO
USB\\VID_1618&PID_C601\\6&4213695&0&1\tQHY600U3G20-20230614\tPCIROOT(0)#PCI(1400)#USBROOT(0)#USB(14)#USB(1)|ACPI(_SB_)#ACPI(PC00)#ACPI(XHCI)#ACPI(RHUB)#ACPI(SS02)#USB(1)\t0\tQHY5IIISeries_IO
USB\\VID_0424&PID_5807\\5&27E528BF&0&14\tUSB5807 Hub\tPCIROOT(0)#PCI(1400)#USBROOT(0)#USB(14)|ACPI(_SB_)#ACPI(PC00)#ACPI(XHCI)#ACPI(RHUB)#ACPI(SS02)\t0\tGeneric SuperSpeed USB Hub
USB\\VID_0403&PID_6015\\UPB248E11M\tUPBv2 revA\tPCIROOT(0)#PCI(1400)#USBROOT(0)#USB(2)#USB(7)|ACPI(_SB_)#ACPI(PC00)#ACPI(XHCI)#ACPI(RHUB)#ACPI(HS02)#USB(7)\t0\tUSB Serial Converter
";

    /// Windows' placeholder for a device whose descriptor request failed is
    /// a fault, and every working device beside it is inventoried.
    #[test]
    fn test_rig2_enumeration_failure_is_a_fault_beside_nine_working_devices() {
        let scan = super::windows::parse_pnp_listing(RIG2_LISTING).unwrap();

        assert_eq!(scan.devices.len(), 9, "every working device is inventoried");
        assert_eq!(
            scan.faults,
            vec![UsbFault {
                record: "USB\\VID_0000&PID_0002\\5&27E528BF&0&5".to_string(),
                vendor: Some("0000".to_string()),
                product: Some("0002".to_string()),
                model: None,
                location: Some(
                    "ACPI(_SB_)#ACPI(PC00)#ACPI(XHCI)#ACPI(RHUB)#ACPI(HS05)".to_string()
                ),
                reason: "Windows reports it not working (problem code 43: Windows stopped it \
                         because it reported problems); Windows lists it as \"Unknown USB \
                         Device (Device Descriptor Request Failed)\""
                    .to_string(),
            }]
        );
    }

    /// With that placeholder fault beside them, the rig's services still
    /// find their devices, matched on the descriptors the devices actually
    /// publish.
    #[test]
    fn test_rig2_services_find_their_devices_despite_the_phantom() {
        let scan = super::windows::parse_pnp_listing(RIG2_LISTING).unwrap();
        let facts = HardwareFacts {
            usb: scan.devices,
            usb_faults: scan.faults,
            ..Default::default()
        };
        assert_eq!(
            facts.usb_present("2e8a", Some("000a"), Some("FP2")),
            Some(true)
        );
        assert_eq!(facts.usb_present("1618", None, None), Some(true));
        assert_eq!(
            facts.usb_present("0403", Some("6015"), Some("UPBv2")),
            Some(true)
        );
    }

    /// A device Windows reports with a problem code is not working — no
    /// driver, failed to start, disabled — so it is a fault even though its
    /// identity and port are known, and the fault keeps both.
    #[test]
    fn test_pnp_device_with_a_problem_code_is_a_fault_that_keeps_its_identity() {
        let listing = "USB\\VID_1618&PID_C601\\6&4213695&0&1\tQHY600U3G20-20230614\t\
                       PCIROOT(0)#PCI(1400)#USBROOT(0)#USB(14)#USB(1)|\
                       ACPI(_SB_)#ACPI(PC00)#ACPI(XHCI)#ACPI(RHUB)#ACPI(SS02)#USB(1)\t\
                       28\tQHY5IIISeries_IO\n";
        let scan = super::windows::parse_pnp_listing(listing).unwrap();

        assert_eq!(scan.devices, Vec::<UsbDevice>::new());
        assert_eq!(
            scan.faults,
            vec![UsbFault {
                record: "USB\\VID_1618&PID_C601\\6&4213695&0&1".to_string(),
                vendor: Some("1618".to_string()),
                product: Some("c601".to_string()),
                model: Some("QHY600U3G20-20230614".to_string()),
                location: Some("PCIROOT(0)#PCI(1400)#USBROOT(0)#USB(14)#USB(1)".to_string()),
                reason: "Windows reports it not working (problem code 28: its drivers are not \
                         installed); Windows lists it as \"QHY5IIISeries_IO\""
                    .to_string(),
            }]
        );
    }

    /// A working device with no `PCIROOT(` path — a Windows ARM host or a
    /// USB-over-IP client would publish one — has no port the inventory can
    /// name, so it is a fault, not a port-less device.
    #[test]
    fn test_pnp_device_without_a_pciroot_path_is_a_fault() {
        let listing = "USB\\VID_03C3&PID_662B\\6&21D0E52E&0&4\tASI662MC\t\
                       ACPI(_SB_)#ACPI(URS0)#USB(4)\t0\tZWO ASI662MC Camera\n";
        let scan = super::windows::parse_pnp_listing(listing).unwrap();

        assert_eq!(scan.devices, Vec::<UsbDevice>::new());
        assert_eq!(scan.faults.len(), 1);
        assert_eq!(
            scan.faults[0].reason,
            "it reports no PCIROOT location path, so its port cannot be named"
        );
        assert_eq!(
            scan.faults[0].location.as_deref(),
            Some("ACPI(_SB_)#ACPI(URS0)#USB(4)"),
            "the only path it has is still where to look"
        );
    }

    /// A location-path read that came back empty says so, rather than
    /// blaming the device for a spelling it may well have.
    #[test]
    fn test_pnp_device_with_unreadable_location_paths_is_a_fault_that_says_so() {
        let listing =
            "USB\\VID_03C3&PID_662B\\6&21D0E52E&0&4\tASI662MC\t\t0\tZWO ASI662MC Camera\n";
        let scan = super::windows::parse_pnp_listing(listing).unwrap();

        assert_eq!(scan.devices, Vec::<UsbDevice>::new());
        assert_eq!(scan.faults.len(), 1);
        assert_eq!(
            scan.faults[0].reason,
            "its location paths could not be read, so its port cannot be named"
        );
        assert_eq!(scan.faults[0].location, None);
    }

    /// A blank problem code — Windows reported none — is not read as
    /// "working": the record is a fault.
    #[test]
    fn test_pnp_device_without_a_problem_code_is_a_fault() {
        let listing = "USB\\VID_03C3&PID_662B\\6&21D0E52E&0&4\tASI662MC\t\
                       PCIROOT(0)#PCI(1400)#USBROOT(0)#USB(2)#USB(4)\t\tZWO ASI662MC Camera\n";
        let scan = super::windows::parse_pnp_listing(listing).unwrap();

        assert_eq!(scan.devices, Vec::<UsbDevice>::new());
        assert_eq!(
            scan.faults[0].reason,
            "Windows did not say whether it is working (no problem code)"
        );
    }

    /// The query prints a problem code as a number or not at all, so a
    /// field that is neither is output the parser does not understand: a
    /// failed scan, not a fault and never a working device.
    #[test]
    fn test_pnp_listing_with_a_non_numeric_problem_code_fails_the_scan() {
        let listing = "USB\\VID_03C3&PID_662B\\6&21D0E52E&0&4\tASI662MC\t\
                       PCIROOT(0)#PCI(1400)#USBROOT(0)#USB(2)#USB(4)\tOK\tZWO ASI662MC Camera\n";
        let error = super::windows::parse_pnp_listing(listing).unwrap_err();
        assert_eq!(
            error,
            "line 1 has a problem code that is not a number (\"OK\")"
        );
    }

    /// The parser's rule that a short or foreign line is a failed scan rests
    /// on the query cleaning the two device-supplied fields: without that, a
    /// tab or line break in one device's product string would fail the
    /// whole listing. The parser tests cannot run `PowerShell`, so this pins
    /// the cleaning in the query text itself — and that its regex reaches
    /// `PowerShell` as `\p{Cc}`, not as a NUL `Command` would refuse.
    #[test]
    fn test_usb_query_cleans_control_characters_from_device_supplied_text() {
        let query = super::windows::USB_QUERY;
        assert!(
            query.contains("$desc = [string](Get-PnpDeviceProperty"),
            "{query}"
        );
        assert!(
            query.contains("$name = [string]$_.FriendlyName -replace '\\p{Cc}', ' '"),
            "{query}"
        );
        assert_eq!(
            query.matches(".Data -replace '\\p{Cc}', ' '").count(),
            1,
            "the description is cleaned: {query}"
        );
        assert!(query.ends_with("`t$code`t$name\" }"), "{query}");
        assert!(!query.contains('\0'), "no NUL reaches the argument");
    }

    /// A per-interface child is skipped whatever its problem field holds:
    /// one composite child must not fail the listing.
    #[test]
    fn test_pnp_listing_skips_an_interface_child_whatever_its_problem_field() {
        let listing = "USB\\VID_2E8A&PID_000A\\E463B0531F4C3831\tDeep Sky Dad FP2\t\
                       PCIROOT(0)#PCI(1400)#USBROOT(0)#USB(1)\t0\tUSB Composite Device\n\
                       USB\\VID_2E8A&PID_000A&MI_00\\7&1A2B3C4D&0&0000\t\t\
                       PCIROOT(0)#PCI(1400)#USBROOT(0)#USB(1)#USBMI(0)\tOK\tx\n";
        let scan = super::windows::parse_pnp_listing(listing).unwrap();
        assert_eq!(scan.faults, Vec::<UsbFault>::new());
        assert_eq!(scan.devices.len(), 1);
    }

    /// A record whose ids cannot be read is a fault whatever its problem
    /// field holds: it is a record the inventory could not identify, not
    /// output it could not read.
    #[test]
    fn test_pnp_listing_reports_unreadable_ids_as_a_fault_whatever_its_problem_field() {
        let listing = "USB\\VID_ZZZZ&PID_0001\\1\t\tPCIROOT(0)#PCI(1400)#USBROOT(0)#USB(1)\tOK\t\n";
        let scan = super::windows::parse_pnp_listing(listing).unwrap();
        assert_eq!(scan.devices, Vec::<UsbDevice>::new());
        assert_eq!(scan.faults.len(), 1);
        assert_eq!(
            scan.faults[0].reason,
            "its instance id does not name a vendor and product"
        );
    }

    /// Every line the query prints is a `USB\VID_` record, so anything else
    /// — here after two good records — fails the scan, naming the line,
    /// instead of reading as a thinner bus.
    #[test]
    fn test_pnp_listing_with_a_line_the_query_cannot_produce_fails_the_scan() {
        let mut listing = RIG2_LISTING.lines().take(2).collect::<Vec<_>>().join("\n");
        listing.push_str("\ngarbage\n");
        let error = super::windows::parse_pnp_listing(&listing).unwrap_err();
        assert_eq!(error, "line 3 is not a USB\\VID_ record: \"garbage\"");
    }

    /// The query prints all five fields for every record, so a record with
    /// fewer is truncated or foreign output: a failed scan, not a fault.
    #[test]
    fn test_pnp_listing_with_a_record_missing_fields_fails_the_scan() {
        let listing = "USB\\VID_03C3&PID_662B\\6&21D0E52E&0&4\tASI662MC\t\
                       PCIROOT(0)#PCI(1400)#USBROOT(0)#USB(2)#USB(4)\n";
        let error = super::windows::parse_pnp_listing(listing).unwrap_err();
        assert!(
            error.starts_with("line 1 has 3 of the 5 tab-separated fields: \"USB\\\\VID_03C3"),
            "{error}"
        );
    }

    /// A line too long to quote whole is cut short in the error.
    #[test]
    fn test_pnp_listing_error_quotes_a_long_line_only_in_part() {
        let listing = "x".repeat(500);
        let error = super::windows::parse_pnp_listing(&listing).unwrap_err();
        assert_eq!(
            error,
            format!("line 1 is not a USB\\VID_ record: \"{}…\"", "x".repeat(120))
        );
    }

    /// An empty listing is an empty bus, which is not an error.
    #[test]
    fn test_pnp_listing_that_is_empty_is_an_empty_bus() {
        let scan = super::windows::parse_pnp_listing("\r\n").unwrap();
        assert_eq!(scan.devices, Vec::<UsbDevice>::new());
        assert_eq!(scan.faults, Vec::<UsbFault>::new());
    }

    /// A registry value under the serial-port key whose data is a string,
    /// as the registry hands it over (terminator included).
    fn port_value(device: &str, data: &str) -> (String, Option<Vec<u16>>) {
        (device.to_string(), Some(data.encode_utf16().collect()))
    }

    /// A registry value of any other type (`REG_DWORD`, `REG_BINARY`, …).
    fn non_string_port_value(device: &str) -> (String, Option<Vec<u16>>) {
        (device.to_string(), None)
    }

    /// Each string value is a port, its terminator and padding stripped.
    #[test]
    fn test_com_port_values_become_trimmed_names() {
        assert_eq!(
            super::windows::port_names([
                port_value(r"\Device\Serial0", "COM3\0"),
                port_value(r"\Device\VCP0", " COM4 \0"),
            ]),
            ["COM3", "COM4"]
        );
    }

    /// A key with no values is a host without COM ports, not an error.
    #[test]
    fn test_com_port_key_without_values_is_no_ports() {
        assert_eq!(super::windows::port_names([]), Vec::<String>::new());
    }

    /// A driver that writes a terminated name into a longer buffer leaves
    /// junk after the NUL; the name is what precedes it.
    #[test]
    fn test_com_port_name_ends_at_its_first_nul() {
        assert_eq!(
            super::windows::port_names([port_value(r"\Device\VCP1", "COM9\0junk\0")]),
            ["COM9"]
        );
    }

    /// A value that is not a string names no port, and costs the listing
    /// nothing else.
    #[test]
    fn test_com_port_value_that_is_not_a_string_is_skipped() {
        assert_eq!(
            super::windows::port_names([
                non_string_port_value(r"\Device\Odd0"),
                port_value(r"\Device\Serial0", "COM3\0"),
            ]),
            ["COM3"]
        );
    }

    #[test]
    fn test_blank_com_port_value_is_skipped() {
        assert_eq!(
            super::windows::port_names([
                port_value(r"\Device\Odd0", " \0"),
                port_value(r"\Device\Serial0", "COM3\0"),
            ]),
            ["COM3"]
        );
    }

    /// A name the collector does not recognise is still a port: no name is
    /// validated, so one odd driver registration cannot fail the listing.
    #[test]
    fn test_com_port_listing_keeps_names_it_does_not_recognise() {
        assert_eq!(
            super::windows::port_names([
                port_value(r"\Device\com0com10", "CNCA0\0"),
                port_value(r"\Device\Serial0", "COM3\0"),
            ]),
            ["CNCA0", "COM3"]
        );
    }

    /// No serial driver has registered a port since boot, so the key does
    /// not exist: the host has no COM ports, which is not a failure.
    #[test]
    fn test_missing_serial_port_key_is_a_host_without_ports() {
        let file_not_found = 0x8007_0002_u32.cast_signed();
        assert_eq!(
            super::windows::listing_without_key(
                file_not_found,
                &"The system cannot find the file specified. (0x80070002)"
            )
            .unwrap(),
            Vec::<String>::new()
        );
    }

    /// Any other reason the key cannot be opened is a listing that failed,
    /// never a host without ports.
    #[test]
    fn test_unopenable_serial_port_key_is_a_failed_listing() {
        let access_denied = 0x8007_0005_u32.cast_signed();
        assert_eq!(
            super::windows::listing_without_key(access_denied, &"Access is denied. (0x80070005)")
                .unwrap_err(),
            COM_LISTING_DENIED
        );
    }

    /// The reason names the step that failed and the key, so an operator
    /// can look at the same key.
    #[test]
    fn test_failed_com_port_listing_names_the_step_and_the_key() {
        assert_eq!(
            super::windows::listing_failed(
                "list the values of",
                &"The handle is invalid. (0x80070006)"
            ),
            r"Windows COM-port listing failed: could not list the values of HKLM\HARDWARE\DEVICEMAP\SERIALCOMM: The handle is invalid. (0x80070006)"
        );
    }

    /// Reads of the serial-port key that answer from `answers` in order,
    /// counting each read.
    fn scripted_reads<'a>(
        answers: Vec<Result<Vec<&'static str>, String>>,
        reads: &'a std::cell::Cell<usize>,
    ) -> impl FnMut() -> Result<Vec<String>, String> + 'a {
        let mut answers = answers.into_iter();
        move || {
            reads.set(reads.get().saturating_add(1));
            answers
                .next()
                .expect("the listing read more often than scripted")
                .map(|ports| ports.into_iter().map(str::to_string).collect())
        }
    }

    /// A second read that agrees confirms the first, and the listing stops
    /// there.
    #[test]
    fn test_com_port_listing_that_reads_the_same_twice_is_settled() {
        let reads = std::cell::Cell::new(0);
        let listing = super::windows::settled(scripted_reads(
            vec![Ok(vec!["COM3", "COM4"]), Ok(vec!["COM3", "COM4"])],
            &reads,
        ));
        assert_eq!(listing.unwrap(), ["COM3", "COM4"]);
        assert_eq!(reads.get(), 2);
    }

    /// A read that a hot-plug tore — here missing a port that never
    /// changed — is not trusted: the listing is the one two reads agree on.
    #[test]
    fn test_torn_com_port_read_is_read_again() {
        let reads = std::cell::Cell::new(0);
        let listing = super::windows::settled(scripted_reads(
            vec![
                Ok(vec!["COM3"]),
                Ok(vec!["COM3", "COM4"]),
                Ok(vec!["COM3", "COM4"]),
            ],
            &reads,
        ));
        assert_eq!(listing.unwrap(), ["COM3", "COM4"]);
        assert_eq!(reads.get(), 3);
    }

    /// A key that never reads the same twice is a listing that failed, not
    /// whichever list the last read happened to return.
    #[test]
    fn test_com_port_listing_that_never_settles_is_a_failed_listing() {
        let reads = std::cell::Cell::new(0);
        let listing = super::windows::settled(scripted_reads(
            vec![Ok(vec!["COM3"]), Ok(vec!["COM3", "COM4"]), Ok(vec!["COM4"])],
            &reads,
        ));
        assert_eq!(
            listing.unwrap_err(),
            r"Windows COM-port listing failed: could not read HKLM\HARDWARE\DEVICEMAP\SERIALCOMM: its values changed on each of 3 reads"
        );
        assert_eq!(reads.get(), 3);
    }

    /// A read that fails fails the listing, whatever an earlier read said.
    #[test]
    fn test_failed_com_port_read_fails_the_listing() {
        let reads = std::cell::Cell::new(0);
        let listing = super::windows::settled(scripted_reads(
            vec![Ok(vec!["COM3"]), Err(COM_LISTING_DENIED.to_string())],
            &reads,
        ));
        assert_eq!(listing.unwrap_err(), COM_LISTING_DENIED);
    }

    /// Only string values carry a port name; an expandable string is read
    /// as it is stored.
    #[cfg(windows)]
    #[test]
    fn test_only_string_registry_values_carry_a_port_name() {
        use windows_registry::{Type, Value};

        let mut expandable = Value::from("COM5");
        expandable.set_ty(Type::ExpandString);
        assert_eq!(
            super::windows::port_names([
                (
                    "string".to_string(),
                    super::windows::port_text(&Value::from("COM3"))
                ),
                (
                    "expandable".to_string(),
                    super::windows::port_text(&expandable)
                ),
                (
                    "dword".to_string(),
                    super::windows::port_text(&Value::from(4_u32))
                ),
            ]),
            ["COM3", "COM5"]
        );
    }

    /// The registry crate's own error for a key that does not exist is the
    /// one the collector reads as a host without ports.
    #[cfg(windows)]
    #[test]
    fn test_registry_error_for_an_absent_key_reads_as_no_ports() {
        let error = windows_registry::LOCAL_MACHINE
            .open(r"HARDWARE\DEVICEMAP\RUSTY_PHOTON_ABSENT_KEY")
            .unwrap_err();
        assert_eq!(
            super::windows::listing_without_key(error.code().0, &error).unwrap(),
            Vec::<String>::new()
        );
    }

    /// The host's real serial-port key, read the way doctor reads it. A
    /// healthy Windows host can always open the key or find it missing;
    /// anything else is the listing failing where nothing is wrong.
    #[cfg(windows)]
    #[test]
    fn test_host_com_port_listing_is_read() {
        let ports = super::windows::com_ports().unwrap();
        for port in &ports {
            assert!(
                !port.is_empty() && port.trim() == port && !port.contains('\0'),
                "port name {port:?} was not cleaned"
            );
        }
    }

    /// The query's `-like 'USB\VID_*'` ignores case, so the parser does too.
    #[test]
    fn test_pnp_listing_accepts_a_lowercase_instance_id() {
        let listing = "usb\\vid_03c3&pid_662b\\6&21d0e52e&0&4\tASI662MC\t\
                       PCIROOT(0)#PCI(1400)#USBROOT(0)#USB(2)#USB(4)\t0\tZWO ASI662MC Camera\n";
        let scan = super::windows::parse_pnp_listing(listing).unwrap();
        assert_eq!(scan.faults, Vec::<UsbFault>::new());
        assert_eq!(scan.devices.len(), 1);
        assert_eq!(scan.devices[0].vendor, "03c3");
        assert_eq!(scan.devices[0].product, "662b");
    }

    #[test]
    fn test_pnp_listing_parses_vid_pid_model_port_and_serial() {
        let devices = super::windows::parse_pnp_listing(RIG2_LISTING)
            .unwrap()
            .devices;
        let upb = devices
            .iter()
            .find(|d| d.vendor == "0403" && d.product == "6015")
            .unwrap();
        assert_eq!(
            upb.model.as_deref(),
            Some("UPBv2 revA"),
            "the model is what the device published on the bus, not the friendly name"
        );
        assert_eq!(
            upb.port.as_deref(),
            Some("PCIROOT(0)#PCI(1400)#USBROOT(0)#USB(2)#USB(7)"),
            "the PCIROOT chain is the port, not the ACPI spelling beside it"
        );
        assert_eq!(
            upb.serial.as_deref(),
            Some("UPB248E11M"),
            "a device that published a serial keeps it in the instance id"
        );

        let qhy600 = devices
            .iter()
            .find(|d| d.product == "c601")
            .expect("instance-id hex normalizes to lowercase");
        assert_eq!(qhy600.vendor, "1618");
        assert_eq!(
            qhy600.port.as_deref(),
            Some("PCIROOT(0)#PCI(1400)#USBROOT(0)#USB(14)#USB(1)")
        );
        assert_eq!(
            qhy600.serial, None,
            "a synthesized parent-relative id encodes the port, not a serial"
        );
    }

    /// An instance id that does not spell both ids as four hex digits is a
    /// record the inventory cannot identify: a fault, not a failed scan, and
    /// never a device with a made-up id.
    #[test]
    fn test_pnp_listing_reports_a_malformed_instance_id_as_a_fault() {
        let listing = "USB\\VID_ZZ\tBroken\tPCIROOT(0)#PCI(1400)#USBROOT(0)#USB(1)\t0\tBroken\n\
                       USB\\VID_03C3&PID_662B\\6&21D0E52E&0&4\tASI662MC\t\
                       PCIROOT(0)#PCI(1400)#USBROOT(0)#USB(2)#USB(4)\t0\tZWO ASI662MC Camera\n";
        let scan = super::windows::parse_pnp_listing(listing).unwrap();

        assert_eq!(
            scan.devices.len(),
            1,
            "the camera beside it is still listed"
        );
        assert_eq!(
            scan.faults,
            vec![UsbFault {
                record: "USB\\VID_ZZ".to_string(),
                vendor: None,
                product: None,
                model: Some("Broken".to_string()),
                location: Some("PCIROOT(0)#PCI(1400)#USBROOT(0)#USB(1)".to_string()),
                reason: "its instance id does not name a vendor and product".to_string(),
            }]
        );
    }

    #[test]
    fn test_pnp_listing_reports_non_hex_ids_as_a_fault() {
        let listing = "USB\\VID_ZZZZ&PID_0001\\1\t\tPCIROOT(0)#PCI(1400)#USBROOT(0)#USB(1)\t0\t\n";
        let scan = super::windows::parse_pnp_listing(listing).unwrap();
        assert_eq!(scan.devices, Vec::<UsbDevice>::new());
        assert_eq!(scan.faults[0].vendor, None);
    }

    /// The collector's output as it arrives: CRLF line endings, captured
    /// verbatim on the rig with the imaging train powered off — the hubs,
    /// the powerbox and the placeholder are what is left on the bus.
    #[test]
    fn test_verbatim_crlf_capture_parses_into_devices_and_the_placeholder_fault() {
        let listing = "USB\\VID_0424&PID_2807\\5&27E528BF&0&2\tUSB2807 Hub\tPCIROOT(0)#PCI(1400)#USBROOT(0)#USB(2)|ACPI(_SB_)#ACPI(PC00)#ACPI(XHCI)#ACPI(RHUB)#ACPI(HS02)\t0\tGeneric USB Hub\r\n\
USB\\VID_0000&PID_0002\\5&27E528BF&0&5\t\tACPI(_SB_)#ACPI(PC00)#ACPI(XHCI)#ACPI(RHUB)#ACPI(HS05)\t43\tUnknown USB Device (Device Descriptor Request Failed)\r\n\
USB\\VID_8087&PID_0033\\5&27E528BF&0&10\t\tPCIROOT(0)#PCI(1400)#USBROOT(0)#USB(10)|ACPI(_SB_)#ACPI(PC00)#ACPI(XHCI)#ACPI(RHUB)#ACPI(HS10)\t0\tIntel(R) Wireless Bluetooth(R)\r\n\
USB\\VID_0424&PID_5807\\5&27E528BF&0&14\tUSB5807 Hub\tPCIROOT(0)#PCI(1400)#USBROOT(0)#USB(14)|ACPI(_SB_)#ACPI(PC00)#ACPI(XHCI)#ACPI(RHUB)#ACPI(SS02)\t0\tGeneric SuperSpeed USB Hub\r\n\
USB\\VID_0403&PID_6015\\UPB248E11M\tUPBv2 revA\tPCIROOT(0)#PCI(1400)#USBROOT(0)#USB(2)#USB(7)|ACPI(_SB_)#ACPI(PC00)#ACPI(XHCI)#ACPI(RHUB)#ACPI(HS02)#USB(7)\t0\tUSB Serial Converter\r\n";
        let scan = super::windows::parse_pnp_listing(listing).unwrap();

        assert_eq!(scan.devices.len(), 4);
        let upb = scan
            .devices
            .iter()
            .find(|d| d.serial.as_deref() == Some("UPB248E11M"))
            .unwrap();
        assert_eq!(
            upb.model.as_deref(),
            Some("UPBv2 revA"),
            "no carriage return survives into a field"
        );
        assert_eq!(scan.faults.len(), 1);
        assert_eq!(
            scan.faults[0].reason,
            "Windows reports it not working (problem code 43: Windows stopped it because it \
             reported problems); Windows lists it as \"Unknown USB Device (Device Descriptor \
             Request Failed)\""
        );
    }

    /// A composite device's per-interface children are functions of the
    /// device listed under its own record, not devices — so a driverless
    /// interface cannot turn a healthy device into a permanent fault.
    #[test]
    fn test_pnp_listing_skips_composite_interface_children() {
        let listing = "USB\\VID_2E8A&PID_000A\\E463B0531F4C3831\tDeep Sky Dad FP2\t\
                       PCIROOT(0)#PCI(1400)#USBROOT(0)#USB(1)\t0\tUSB Composite Device\n\
                       USB\\VID_2E8A&PID_000A&MI_00\\7&1A2B3C4D&0&0000\t\t\
                       PCIROOT(0)#PCI(1400)#USBROOT(0)#USB(1)#USBMI(0)\t0\tUSB Serial Device (COM4)\n\
                       USB\\VID_2E8A&PID_000A&MI_02\\7&1A2B3C4D&0&0002\t\t\
                       PCIROOT(0)#PCI(1400)#USBROOT(0)#USB(1)#USBMI(2)\t28\tReset\n";
        let scan = super::windows::parse_pnp_listing(listing).unwrap();

        assert_eq!(scan.faults, Vec::<UsbFault>::new());
        assert_eq!(scan.devices.len(), 1);
        assert_eq!(
            scan.devices[0].port.as_deref(),
            Some("PCIROOT(0)#PCI(1400)#USBROOT(0)#USB(1)")
        );
    }

    /// A code Device Manager's table does not cover is still a fault, named
    /// by its number.
    #[test]
    fn test_pnp_device_with_an_unlisted_problem_code_is_a_fault_named_by_number() {
        let listing = "USB\\VID_03C3&PID_662B\\6&21D0E52E&0&4\tASI662MC\t\
                       PCIROOT(0)#PCI(1400)#USBROOT(0)#USB(2)#USB(4)\t99\t\n";
        let scan = super::windows::parse_pnp_listing(listing).unwrap();

        assert_eq!(scan.devices, Vec::<UsbDevice>::new());
        assert_eq!(
            scan.faults[0].reason,
            "Windows reports it not working (problem code 99)"
        );
    }

    fn system_profiler(json: &str) -> Result<UsbScan, String> {
        super::macos::parse_system_profiler(&serde_json::from_str(json).unwrap())
    }

    /// A report without the `SPUSBDataType` list — the shape expected
    /// (unverified) from a `system_profiler` that no longer answers for the
    /// data type — fails the scan instead of reading as a bus with no
    /// devices.
    #[test]
    fn test_system_profiler_report_without_the_usb_list_fails_the_scan() {
        assert_eq!(
            system_profiler("{}").unwrap_err(),
            "it has no SPUSBDataType list"
        );
        assert_eq!(
            system_profiler(r#"{ "SPUSBDataType": {} }"#).unwrap_err(),
            "it has no SPUSBDataType list",
            "the data type must be a list"
        );
    }

    /// An empty list is a genuinely empty bus, which is not an error.
    #[test]
    fn test_system_profiler_empty_usb_list_is_an_empty_bus() {
        let scan = system_profiler(r#"{ "SPUSBDataType": [] }"#).unwrap();
        assert_eq!(scan.devices, Vec::<UsbDevice>::new());
        assert_eq!(scan.faults, Vec::<UsbFault>::new());
    }

    /// A device behind a hub is found by recursing into `_items`, and its
    /// port is the location id's hex without the attach-order address.
    #[test]
    fn test_system_profiler_walk_lists_a_nested_device_with_its_port() {
        let scan = system_profiler(
            r#"{ "SPUSBDataType": [ { "_name": "USB31Bus", "_items": [
                { "_name": "USB2.0 Hub", "vendor_id": "0x05e3", "product_id": "0x0610",
                  "location_id": "0x14200000 / 2",
                  "_items": [ { "_name": "ASI662MC", "vendor_id": "0x03c3  (ZWO)",
                                "product_id": "0x662b", "location_id": "0x14210000 / 3",
                                "serial_num": "" } ] } ] } ] }"#,
        )
        .unwrap();

        assert_eq!(scan.faults, Vec::<UsbFault>::new());
        assert_eq!(scan.devices.len(), 2, "the controller node is not a device");
        let camera = scan.devices.iter().find(|d| d.vendor == "03c3").unwrap();
        assert_eq!(camera.product, "662b");
        assert_eq!(camera.model.as_deref(), Some("ASI662MC"));
        assert_eq!(camera.port.as_deref(), Some("0x14210000"));
        assert_eq!(camera.serial, None, "an empty serial is no serial");
    }

    /// A device with no usable location id has no port the inventory can
    /// name: a fault, and the device beside it is still listed.
    #[test]
    fn test_system_profiler_device_without_a_location_id_is_a_fault() {
        let scan = system_profiler(
            r#"{ "SPUSBDataType": [ { "_name": "USB31Bus", "_items": [
                { "_name": "UPBv2 revA", "vendor_id": "0x0403", "product_id": "0x6015",
                  "location_id": "0x14100000 / 1" },
                { "_name": "ASI662MC", "vendor_id": "0x03c3", "product_id": "0x662b" } ] } ] }"#,
        )
        .unwrap();

        assert_eq!(scan.devices.len(), 1);
        assert_eq!(scan.devices[0].vendor, "0403");
        assert_eq!(
            scan.faults,
            vec![UsbFault {
                record: "ASI662MC".to_string(),
                vendor: Some("03c3".to_string()),
                product: Some("662b".to_string()),
                model: Some("ASI662MC".to_string()),
                location: None,
                reason: "it reports no usable location_id, so its port cannot be named".to_string(),
            }]
        );
    }

    /// A fault with no name to go by is still identifiable by its ids.
    #[test]
    fn test_system_profiler_fault_without_a_name_is_recorded_by_its_ids() {
        let scan = system_profiler(
            r#"{ "SPUSBDataType": [ { "vendor_id": "0x03c3", "product_id": "0x662b",
                                      "location_id": "garbage" } ] }"#,
        )
        .unwrap();
        assert_eq!(scan.faults[0].record, "03c3:662b");
        assert_eq!(scan.faults[0].model, None);
    }

    /// The staged USB inventory — docs/services/doctor.md, "USB inventory".
    #[cfg(feature = "mock")]
    mod staged_inventory {
        use std::path::{Path, PathBuf};

        use super::super::{
            gather, HardwareFacts, ProbeRequest, StagedUsbInventory, UsbDevice, UsbFault,
        };

        fn stage(dir: &Path, json: &str) -> PathBuf {
            let path = dir.join("inventory.json");
            std::fs::write(&path, json).unwrap();
            path
        }

        fn request(staged: StagedUsbInventory) -> ProbeRequest {
            ProbeRequest {
                service_user: "rusty-photon".to_string(),
                staged_usb: Some(staged),
                ..Default::default()
            }
        }

        /// The rig2 placeholder as the Windows collector reports it.
        fn phantom() -> UsbFault {
            UsbFault {
                record: "USB\\VID_0000&PID_0002\\5&27E528BF&0&5".to_string(),
                vendor: Some("0000".to_string()),
                product: Some("0002".to_string()),
                model: None,
                location: Some(
                    "ACPI(_SB_)#ACPI(PC00)#ACPI(XHCI)#ACPI(RHUB)#ACPI(HS05)".to_string(),
                ),
                reason: "Windows reports it not working (problem code 43: Windows stopped it \
                         because it reported problems); Windows lists it as \"Unknown USB \
                         Device (Device Descriptor Request Failed)\""
                    .to_string(),
            }
        }

        /// A staged fault lands beside the staged devices, exactly as a
        /// collector reports one: out of the inventory, and costing nothing
        /// else.
        #[test]
        fn test_a_staged_fault_reaches_the_facts_beside_the_devices() {
            let dir = tempfile::tempdir().unwrap();
            let path = stage(
                dir.path(),
                r#"{ "usb": [ { "vendor": "2e8a", "product": "000a",
                                "model": "Deep Sky Dad FP2",
                                "port": "PCIROOT(0)#PCI(1400)#USBROOT(0)#USB(1)" } ],
                     "usb_faults": [ { "record": "USB\\VID_0000&PID_0002\\5&27E528BF&0&5",
                                       "vendor": "0000", "product": "0002",
                                       "location": "ACPI(_SB_)#ACPI(PC00)#ACPI(XHCI)#ACPI(RHUB)#ACPI(HS05)",
                                       "reason": "Windows reports it not working (problem code 43: Windows stopped it because it reported problems); Windows lists it as \"Unknown USB Device (Device Descriptor Request Failed)\"" } ] }"#,
            );
            let facts = gather(&request(StagedUsbInventory::load(&path).unwrap()));
            assert_eq!(facts.usb_faults, vec![phantom()]);
            assert_eq!(
                facts.usb_present("2e8a", Some("000a"), Some("FP2")),
                Some(true)
            );
            assert!(facts.usb_unavailable.is_none());
        }

        /// A failed scan reports neither devices nor faults.
        #[test]
        fn test_a_document_naming_both_a_failure_and_a_fault_is_rejected() {
            let dir = tempfile::tempdir().unwrap();
            let path = stage(
                dir.path(),
                r#"{ "usb_unavailable": "powershell.exe did not finish within 10s",
                     "usb_faults": [ { "record": "USB\\VID_0000&PID_0002\\1", "reason": "dead" } ] }"#,
            );
            let error = StagedUsbInventory::load(&path).unwrap_err();
            assert!(error.contains("no faults"), "{error}");
        }

        /// Faults alone do not say what is on the bus: a scan that found
        /// only faults still reports its empty device list.
        #[test]
        fn test_faults_alone_do_not_state_a_bus() {
            let dir = tempfile::tempdir().unwrap();
            let path = stage(
                dir.path(),
                r#"{ "usb_faults": [ { "record": "1-9", "reason": "dead" } ] }"#,
            );
            let error = StagedUsbInventory::load(&path).unwrap_err();
            assert!(error.contains("states neither"), "{error}");
        }

        /// The least a collector reports about a fault is its name and why
        /// — a record whose identity could not be read has nothing else.
        #[test]
        fn test_a_fault_with_only_a_record_and_a_reason_is_stageable() {
            let dir = tempfile::tempdir().unwrap();
            let path = stage(
                dir.path(),
                r#"{ "usb": [], "usb_faults": [ { "record": "USB\\VID_ZZ",
                     "reason": "its instance id does not name a vendor and product" } ] }"#,
            );
            let staged = StagedUsbInventory::load(&path).unwrap();
            let StagedUsbInventory::Scan { faults, .. } = staged else {
                panic!("expected a scan, got {staged:?}");
            };
            assert_eq!(faults.len(), 1);
            assert_eq!(faults[0].vendor, None);
        }

        /// Doctor prints the reason — a fault without one could not tell the
        /// operator what is wrong.
        #[test]
        fn test_a_staged_fault_with_a_blank_reason_is_rejected() {
            let dir = tempfile::tempdir().unwrap();
            let path = stage(
                dir.path(),
                r#"{ "usb": [], "usb_faults": [ { "record": "1-9", "reason": " " } ] }"#,
            );
            let error = StagedUsbInventory::load(&path).unwrap_err();
            assert!(error.contains("blank `reason`"), "{error}");
        }

        /// A fault's ids follow the device rules: the one spelling every
        /// collector reports.
        #[test]
        fn test_a_staged_fault_with_an_uppercase_id_is_rejected() {
            let dir = tempfile::tempdir().unwrap();
            let path = stage(
                dir.path(),
                r#"{ "usb": [], "usb_faults": [ { "record": "USB\\VID_1618&PID_C601\\1",
                     "vendor": "1618", "product": "C601", "reason": "dead" } ] }"#,
            );
            let error = StagedUsbInventory::load(&path).unwrap_err();
            assert!(error.contains("four lowercase hex digits"), "{error}");
        }

        /// So does every text field: a padded location is a spelling no
        /// collector produces.
        #[test]
        fn test_a_staged_fault_with_a_padded_location_is_rejected() {
            let dir = tempfile::tempdir().unwrap();
            let path = stage(
                dir.path(),
                r#"{ "usb": [], "usb_faults": [ { "record": "1-9", "location": "1-9 ",
                     "reason": "dead" } ] }"#,
            );
            let error = StagedUsbInventory::load(&path).unwrap_err();
            assert!(error.contains("padded `location`"), "{error}");
        }

        /// The vendor follows the same rule as the product: a fault staged
        /// with the `Get-PnpDevice` spelling would never match the service
        /// it names.
        #[test]
        fn test_a_staged_fault_with_an_uppercase_vendor_is_rejected() {
            let dir = tempfile::tempdir().unwrap();
            let path = stage(
                dir.path(),
                r#"{ "usb": [], "usb_faults": [ { "record": "USB\\VID_2E8A&PID_000A\\1",
                     "vendor": "2E8A", "product": "000a", "reason": "dead" } ] }"#,
            );
            let error = StagedUsbInventory::load(&path).unwrap_err();
            assert!(error.contains("`vendor`"), "{error}");
            assert!(error.contains("four lowercase hex digits"), "{error}");
        }

        /// Doctor prints the record so the operator can find the device — a
        /// fault without one could not be found.
        #[test]
        fn test_a_staged_fault_with_a_blank_record_is_rejected() {
            let dir = tempfile::tempdir().unwrap();
            let path = stage(
                dir.path(),
                r#"{ "usb": [], "usb_faults": [ { "record": "", "reason": "dead" } ] }"#,
            );
            let error = StagedUsbInventory::load(&path).unwrap_err();
            assert!(error.contains("blank `record`"), "{error}");
        }

        /// Replaces the scan rather than adding to it: the gathered bus is
        /// exactly what was staged, on a dev box whose own bus is not.
        #[test]
        fn test_a_staged_device_list_replaces_the_host_scan() {
            let dir = tempfile::tempdir().unwrap();
            let path = stage(
                dir.path(),
                r#"{ "usb": [ { "vendor": "1618", "product": "c601",
                     "model": "QHY600U3G20-20230614",
                     "port": "PCIROOT(0)#PCI(1400)#USBROOT(0)#USB(14)#USB(1)" } ] }"#,
            );
            let facts = gather(&request(StagedUsbInventory::load(&path).unwrap()));
            assert_eq!(facts.usb.len(), 1);
            assert_eq!(facts.usb[0].vendor, "1618");
            assert_eq!(
                facts.usb[0].port.as_deref(),
                Some("PCIROOT(0)#PCI(1400)#USBROOT(0)#USB(14)#USB(1)")
            );
            assert!(facts.usb_unavailable.is_none());
            assert_eq!(facts.usb_present("1618", Some("c601"), None), Some(true));
        }

        /// Staging bypasses the inventory and nothing else: the same
        /// gather still answers what the request asked about the host, so a
        /// scenario cannot read a staged inventory as staged facts.
        #[test]
        fn test_staging_the_inventory_leaves_the_rest_of_the_gather_alone() {
            let dir = tempfile::tempdir().unwrap();
            let probed = dir.path().join("probed");
            std::fs::write(&probed, b"x").unwrap();
            let path = stage(
                dir.path(),
                r#"{ "usb": [ { "vendor": "1618", "product": "c601", "port": "1-4.2" } ] }"#,
            );
            let mut req = request(StagedUsbInventory::load(&path).unwrap());
            req.paths = vec![probed.clone()];
            let facts = gather(&req);
            assert_eq!(facts.usb.len(), 1);
            assert!(facts.paths.contains_key(probed.to_str().unwrap()));
        }

        /// A staged failure is a failure, not an idle bus: the marker
        /// survives to the facts and the presence question answers nothing.
        #[test]
        fn test_a_staged_failure_reaches_the_facts_as_unavailable() {
            let dir = tempfile::tempdir().unwrap();
            let path = stage(
                dir.path(),
                r#"{ "usb_unavailable": "system_profiler did not finish within 10s" }"#,
            );
            let facts = gather(&request(StagedUsbInventory::load(&path).unwrap()));
            assert_eq!(facts.usb, Vec::<UsbDevice>::new());
            assert_eq!(
                facts.usb_unavailable.as_deref(),
                Some("system_profiler did not finish within 10s")
            );
            assert_eq!(facts.usb_present("1618", None, None), None);
        }

        /// A failed scan has no opinion about what is on the bus, so a
        /// document claiming both states describes nothing a collector could
        /// have produced.
        #[test]
        fn test_a_document_naming_both_a_failure_and_a_device_is_rejected() {
            let dir = tempfile::tempdir().unwrap();
            let path = stage(
                dir.path(),
                r#"{ "usb_unavailable": "sysfs unreadable",
                     "usb": [ { "vendor": "1618", "product": "c601", "port": "1-4.2" } ] }"#,
            );
            let error = StagedUsbInventory::load(&path).unwrap_err();
            assert!(
                error.contains("a failed scan reports no devices"),
                "{error}"
            );
        }

        /// A gathered candidate without a port is a fault, never a device,
        /// so staging one would let a scenario assert on a state the runtime
        /// cannot produce. The message says what to stage instead.
        #[test]
        fn test_a_staged_device_without_a_port_is_rejected() {
            let dir = tempfile::tempdir().unwrap();
            let path = stage(
                dir.path(),
                r#"{ "usb": [ { "vendor": "1618", "product": "c601" } ] }"#,
            );
            let error = StagedUsbInventory::load(&path).unwrap_err();
            assert!(error.contains("1618:c601"), "{error}");
            assert!(error.contains("usb_faults"), "{error}");
        }

        /// An empty bus is a state every collector can report, so it stays
        /// stageable — `Ok(empty)` means a genuinely idle bus, which is how
        /// a claimed port with nothing in it gets exercised.
        #[test]
        fn test_an_explicitly_empty_bus_is_an_empty_bus() {
            let dir = tempfile::tempdir().unwrap();
            let path = stage(dir.path(), r#"{ "usb": [] }"#);
            let facts = gather(&request(StagedUsbInventory::load(&path).unwrap()));
            assert_eq!(facts.usb, Vec::<UsbDevice>::new());
            assert!(facts.usb_unavailable.is_none());
            assert_eq!(facts.usb_present("1618", None, None), Some(false));
        }

        /// A nulled list is neither an empty bus nor an omitted key, and
        /// no capture produces it: `HardwareFacts` holds `usb` as a `Vec`,
        /// so a serialized one always carries a list. Rejected rather than
        /// folded into "absent", so the document format has one meaning per
        /// spelling.
        #[test]
        fn test_a_nulled_device_list_is_rejected() {
            let dir = tempfile::tempdir().unwrap();
            let path = stage(
                dir.path(),
                r#"{ "usb": null, "usb_unavailable": "sysfs gone" }"#,
            );
            let error = StagedUsbInventory::load(&path).unwrap_err();
            assert!(error.contains("`usb` is null"), "{error}");
        }

        /// The asymmetry is deliberate and has a reason: `usb_unavailable`
        /// is an `Option` on `HardwareFacts`, so `null` there is how a
        /// *successful* scan serializes — rejecting it would break staging
        /// a real capture, which the round-trip test above pins.
        #[test]
        fn test_a_nulled_failure_reason_is_a_successful_scan() {
            let dir = tempfile::tempdir().unwrap();
            let path = stage(dir.path(), r#"{ "usb": [], "usb_unavailable": null }"#);
            let staged = StagedUsbInventory::load(&path).unwrap();
            assert_eq!(
                staged,
                StagedUsbInventory::Scan {
                    devices: Vec::new(),
                    faults: Vec::new()
                }
            );
        }

        /// But a document that mentions neither key states nothing, and
        /// nothing is not empty — the distinction this whole type exists
        /// for. A staging file that failed to be written must not read as an
        /// idle bus and let a scenario pass for the wrong reason.
        #[test]
        fn test_a_document_stating_neither_is_rejected() {
            let dir = tempfile::tempdir().unwrap();
            let path = stage(dir.path(), "{}");
            let error = StagedUsbInventory::load(&path).unwrap_err();
            assert!(error.contains("states neither"), "{error}");
        }

        /// The quiet copy-paste: `Get-PnpDevice` prints `USB\VID_1618&PID_C601`,
        /// and an id taken from it verbatim compares unequal to the
        /// lowercase form every collector reports — forever, and silently.
        #[test]
        fn test_a_staged_device_with_an_uppercase_id_is_rejected() {
            let dir = tempfile::tempdir().unwrap();
            let path = stage(
                dir.path(),
                r#"{ "usb": [ { "vendor": "1618", "product": "C601", "port": "1-4.2" } ] }"#,
            );
            let error = StagedUsbInventory::load(&path).unwrap_err();
            assert!(error.contains("four lowercase hex digits"), "{error}");
        }

        /// The other spelling a human reaches for: macOS prints `0x1618`,
        /// and its own reader strips the prefix before storing.
        #[test]
        fn test_a_staged_device_with_a_prefixed_id_is_rejected() {
            let dir = tempfile::tempdir().unwrap();
            let path = stage(
                dir.path(),
                r#"{ "usb": [ { "vendor": "0x1618", "product": "c601", "port": "1-4.2" } ] }"#,
            );
            let error = StagedUsbInventory::load(&path).unwrap_err();
            assert!(error.contains("four lowercase hex digits"), "{error}");
        }

        /// `Some("")` is the absence of a descriptor wearing the shape of
        /// one. All three collectors return `None` for an unreadable
        /// descriptor, so a staged blank describes no reachable state.
        #[test]
        fn test_a_staged_device_with_a_blank_model_is_rejected() {
            let dir = tempfile::tempdir().unwrap();
            let path = stage(
                dir.path(),
                r#"{ "usb": [ { "vendor": "1618", "product": "c601", "port": "1-4.2",
                     "model": "" } ] }"#,
            );
            let error = StagedUsbInventory::load(&path).unwrap_err();
            assert!(error.contains("blank `model`"), "{error}");
            assert!(error.contains("null"), "{error}");
        }

        /// The capture-and-stage property, end to end: what `HardwareFacts`
        /// serializes is what this loader accepts. A *successful* scan
        /// serializes `"usb_unavailable": null`, which is why null there
        /// means "no failure" rather than "a failure with no reason" —
        /// reading it the other way would make a healthy rig's own facts
        /// file unstageable, and this document uses those field names
        /// precisely so it does not have to be rewritten by hand.
        #[test]
        fn test_serialized_hardware_facts_stage_verbatim() {
            let facts = HardwareFacts {
                usb: vec![UsbDevice {
                    vendor: "03c3".to_string(),
                    product: "662b".to_string(),
                    model: Some("ASI662MC".to_string()),
                    port: Some("1-4.2".to_string()),
                    serial: None,
                }],
                usb_faults: vec![phantom()],
                ..Default::default()
            };
            let document = serde_json::to_string(&facts).unwrap();
            assert!(
                document.contains(r#""usb_unavailable":null"#),
                "a successful scan serializes an explicit null: {document}"
            );
            let dir = tempfile::tempdir().unwrap();
            let path = stage(dir.path(), &document);
            assert_eq!(
                StagedUsbInventory::load(&path).unwrap(),
                StagedUsbInventory::Scan {
                    devices: facts.usb,
                    faults: facts.usb_faults
                }
            );
        }

        /// But an explicit `null` is a state collectors reach constantly —
        /// on rig2 not one of the three cameras publishes a USB serial — so
        /// it stays accepted, and a captured facts file full of them stages
        /// as it stands.
        #[test]
        fn test_explicit_nulls_are_a_device_that_published_neither() {
            let dir = tempfile::tempdir().unwrap();
            let path = stage(
                dir.path(),
                r#"{ "usb": [ { "vendor": "1618", "product": "c601", "port": "1-4.2",
                     "model": null, "serial": null } ] }"#,
            );
            let facts = gather(&request(StagedUsbInventory::load(&path).unwrap()));
            assert_eq!(facts.usb.len(), 1);
            assert!(facts.usb[0].model.is_none());
            assert!(facts.usb[0].serial.is_none());
        }

        /// Padding is the quieter cousin of a blank value: it passes a
        /// non-empty check and then matches nothing. Every collector trims,
        /// so no staged document should carry it — on the join keys or on
        /// the descriptor fields.
        #[test]
        fn test_a_staged_device_with_a_padded_port_is_rejected() {
            let dir = tempfile::tempdir().unwrap();
            let path = stage(
                dir.path(),
                r#"{ "usb": [ { "vendor": "1618", "product": "c601", "port": " 1-4.2" } ] }"#,
            );
            let error = StagedUsbInventory::load(&path).unwrap_err();
            assert!(error.contains("padded `port`"), "{error}");
        }

        #[test]
        fn test_a_staged_device_with_a_padded_vendor_is_rejected() {
            let dir = tempfile::tempdir().unwrap();
            let path = stage(
                dir.path(),
                r#"{ "usb": [ { "vendor": "1618\n", "product": "c601", "port": "1-4.2" } ] }"#,
            );
            let error = StagedUsbInventory::load(&path).unwrap_err();
            assert!(error.contains("padded `vendor`"), "{error}");
        }

        #[test]
        fn test_a_staged_device_with_a_padded_serial_is_rejected() {
            let dir = tempfile::tempdir().unwrap();
            let path = stage(
                dir.path(),
                r#"{ "usb": [ { "vendor": "1618", "product": "c601", "port": "1-4.2",
                     "serial": "UPB248E11M " } ] }"#,
            );
            let error = StagedUsbInventory::load(&path).unwrap_err();
            assert!(error.contains("padded `serial`"), "{error}");
        }

        /// Blank is not `None`, and it is the worse absence: an empty port
        /// matches nothing while reading like a device that simply did not
        /// match a claim. No collector emits one.
        #[test]
        fn test_a_staged_device_with_a_blank_port_is_rejected() {
            let dir = tempfile::tempdir().unwrap();
            let path = stage(
                dir.path(),
                r#"{ "usb": [ { "vendor": "1618", "product": "c601", "port": "  " } ] }"#,
            );
            let error = StagedUsbInventory::load(&path).unwrap_err();
            assert!(error.contains("1618:c601"), "{error}");
            assert!(error.contains("has no port"), "{error}");
        }

        /// `product` defaults to an empty string when the key is absent, so
        /// an omitted one is silent rather than a parse error — and a
        /// collector reports it for every device in the inventory, while a
        /// record whose product it cannot read is a fault.
        #[test]
        fn test_a_staged_device_without_a_product_is_rejected() {
            let dir = tempfile::tempdir().unwrap();
            let path = stage(
                dir.path(),
                r#"{ "usb": [ { "vendor": "1618", "port": "1-4.2" } ] }"#,
            );
            let error = StagedUsbInventory::load(&path).unwrap_err();
            assert!(error.contains("missing a vendor or product id"), "{error}");
        }

        /// A failure doctor cannot explain sends an operator nowhere.
        #[test]
        fn test_a_staged_failure_without_a_reason_is_rejected() {
            let dir = tempfile::tempdir().unwrap();
            let path = stage(dir.path(), r#"{ "usb_unavailable": "" }"#);
            let error = StagedUsbInventory::load(&path).unwrap_err();
            assert!(error.contains("no reason"), "{error}");
        }

        /// The document is `HardwareFacts`' own field names, so the
        /// `hardware` object of a facts file captured from a real rig stages
        /// as it stands — every other key in it ignored.
        #[test]
        fn test_a_captured_hardware_object_stages_unchanged() {
            let dir = tempfile::tempdir().unwrap();
            let path = stage(
                dir.path(),
                r#"{ "paths": {}, "com_ports": [],
                     "com_ports_unavailable": "Windows COM-port listing failed: could not open the key",
                     "groups": { "plugdev": 46 },
                     "udev_rules": {},
                     "usb": [ { "vendor": "03c3", "product": "662b",
                                "model": "ASI662MC", "port": "1-4.2",
                                "serial": null } ] }"#,
            );
            let staged = StagedUsbInventory::load(&path).unwrap();
            assert_eq!(
                staged,
                StagedUsbInventory::Scan {
                    devices: vec![UsbDevice {
                        vendor: "03c3".to_string(),
                        product: "662b".to_string(),
                        model: Some("ASI662MC".to_string()),
                        port: Some("1-4.2".to_string()),
                        serial: None,
                    }],
                    faults: Vec::new(),
                }
            );
        }

        /// A staging path that cannot be read is a broken scenario, never a
        /// simulated bus failure — it must not be mistakable for one.
        #[test]
        fn test_an_absent_file_names_the_path_it_could_not_read() {
            let dir = tempfile::tempdir().unwrap();
            let missing = dir.path().join("nothing.json");
            let error = StagedUsbInventory::load(&missing).unwrap_err();
            assert!(
                error.contains("could not read staged USB inventory"),
                "{error}"
            );
            assert!(error.contains("nothing.json"), "{error}");
        }
    }

    /// Fixtures for the bounded-subprocess helper. It ships on macOS and
    /// Windows, so the tests run on both rather than on the Unix family
    /// alone — the platforms differ in exactly the mechanics under test
    /// (spawn, pipe, kill), which is what makes a Unix-only pass a weak
    /// signal for the Windows collector.
    #[cfg(any(unix, windows))]
    mod bounded_capture {
        use std::process::Command;
        use std::time::{Duration, Instant};

        use super::super::bounded;

        /// Scripts, not programs: "write this, then exit like that" has no
        /// portable single binary, and the two shells spell it differently.
        #[cfg(unix)]
        const WRITE_HELLO: &str = "printf hello";
        #[cfg(windows)]
        const WRITE_HELLO: &str = "echo hello";

        #[cfg(unix)]
        const WRITE_THEN_FAIL: &str = "printf partial; exit 3";
        #[cfg(windows)]
        const WRITE_THEN_FAIL: &str = "echo partial & exit 3";

        #[cfg(unix)]
        const NEVER_FINISHES: &str = "sleep 30";
        #[cfg(windows)]
        const NEVER_FINISHES: &str = "ping -n 31 127.0.0.1 >nul";

        /// Waits, then leaves a mark. Killed on time, the mark never appears.
        #[cfg(unix)]
        const WAIT_THEN_MARK: &str = "sleep 1; : > marker";
        #[cfg(windows)]
        const WAIT_THEN_MARK: &str = "ping -n 3 127.0.0.1 >nul & echo . > marker";

        #[cfg(unix)]
        const COPY_BULK: &str = "cat bulk";
        #[cfg(windows)]
        const COPY_BULK: &str = "type bulk";

        #[cfg(unix)]
        fn shell(script: &str) -> Command {
            let mut cmd = Command::new("/bin/sh");
            cmd.args(["-c", script]);
            cmd
        }

        #[cfg(windows)]
        fn shell(script: &str) -> Command {
            let mut cmd = Command::new("cmd");
            cmd.args(["/C", script]);
            cmd
        }

        /// The success path, and with it `reap`: a child that writes and
        /// exits hands back what it wrote.
        #[test]
        fn test_capture_returns_what_the_child_wrote() {
            let output = bounded::capture(&mut shell(WRITE_HELLO), bounded::DEADLINE).unwrap();
            // Trimmed: `cmd`'s `echo` appends a newline where `printf` does
            // not, and which one ran is not what this test is about.
            assert_eq!(String::from_utf8_lossy(&output).trim(), "hello");
        }

        /// Why the deadline is on the *drain* and not on the child exiting:
        /// a child whose output exceeds the pipe buffer blocks writing until
        /// someone reads, so a wait-then-read implementation deadlocks here.
        #[test]
        fn test_capture_drains_more_than_one_pipe_buffer() {
            let dir = tempfile::tempdir().unwrap();
            std::fs::write(dir.path().join("bulk"), vec![b'x'; 200_000]).unwrap();
            let mut cmd = shell(COPY_BULK);
            cmd.current_dir(dir.path());
            let output = bounded::capture(&mut cmd, bounded::DEADLINE).unwrap();
            // Trimmed rather than length-matched: a shell may add a line
            // ending of its own, and the claim under test is that none of
            // the payload was lost.
            let payload = output.trim_ascii();
            assert_eq!(payload.len(), 200_000);
            assert!(payload.iter().all(|b| *b == b'x'));
        }

        /// A child that fails is a failed scan, not a short one — its partial
        /// output must never reach a caller as an inventory.
        #[test]
        fn test_capture_rejects_a_nonzero_exit_and_its_output() {
            let error =
                bounded::capture(&mut shell(WRITE_THEN_FAIL), bounded::DEADLINE).unwrap_err();
            assert!(error.contains("exited with"), "{error}");
        }

        /// The wedged-collector case the deadline exists for: an error rather
        /// than a hung startup.
        #[test]
        fn test_capture_gives_up_on_a_child_that_never_finishes() {
            let error =
                bounded::capture(&mut shell(NEVER_FINISHES), Duration::from_secs(2)).unwrap_err();
            assert!(error.contains("did not finish within"), "{error}");
        }

        /// The grace period itself, which no `capture` fixture reaches: the
        /// deadline one never gets past `recv_timeout`, and the success one
        /// exits before the first poll. So the guard this commit changed is
        /// pinned directly — a child that lingers gets the grace and no more.
        #[test]
        fn test_reap_waits_out_the_grace_period_then_gives_up() {
            let mut cmd = shell(NEVER_FINISHES);
            cmd.stdout(std::process::Stdio::null());
            let mut child = cmd.spawn().unwrap();
            let started = Instant::now();
            let error = bounded::reap(&mut child).unwrap_err();
            let waited = started.elapsed();
            assert!(error.contains("did not exit"), "{error}");
            // Bounded both ways, because both regressions are silent: a guard
            // that never waits returns at once, and one that never expires
            // hangs here rather than reporting.
            assert!(waited >= bounded::REAP_GRACE, "gave up after {waited:?}");
            assert!(waited < Duration::from_secs(5), "waited {waited:?}");
        }

        /// And it kills what it gave up on. An orphan would outlive the
        /// scan, still holding whatever it had open — observable here as the
        /// mark it would have left after the deadline had passed.
        #[test]
        fn test_capture_kills_the_child_it_gave_up_on() {
            let dir = tempfile::tempdir().unwrap();
            let mut cmd = shell(WAIT_THEN_MARK);
            cmd.current_dir(dir.path());
            bounded::capture(&mut cmd, Duration::from_millis(200)).unwrap_err();
            // Well past when the mark would have been written had the child
            // survived the deadline.
            std::thread::sleep(Duration::from_secs(3));
            assert!(
                !dir.path().join("marker").exists(),
                "the child outlived the deadline that killed it"
            );
        }
    }

    #[test]
    fn test_gather_answers_only_what_was_asked() {
        let dir = tempfile::tempdir().unwrap();
        let probed = dir.path().join("probed");
        std::fs::write(&probed, "x").unwrap();
        let req = ProbeRequest {
            paths: vec![probed.clone(), dir.path().join("absent")],
            service_user: "rusty-photon".to_string(),
            ..Default::default()
        };
        let facts = gather(&req);
        assert!(facts
            .paths
            .contains_key(&probed.to_string_lossy().into_owned()));
        assert!(!facts
            .paths
            .contains_key(&dir.path().join("absent").to_string_lossy().into_owned()));
    }

    /// `gather` lands the host's COM-port listing as the collector reads
    /// it. On a host without COM ports both sides are empty, so this proves
    /// the wiring only where the host lists at least one port.
    #[cfg(windows)]
    #[test]
    fn test_gather_lands_the_host_com_port_listing() {
        let facts = gather(&ProbeRequest::default());
        assert_eq!(facts.com_ports_unavailable, None);
        assert_eq!(facts.com_ports, super::windows::com_ports().unwrap());
    }
}
