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
    /// candidate record without a port is an inventory failure, because a
    /// port-less candidate is indistinguishable from one whose port simply
    /// did not match.
    #[serde(default)]
    pub port: Option<String>,
    /// The serial the device publishes on the bus, when it publishes one.
    /// Many cameras publish none — an absent serial is normal and says
    /// nothing about whether the device has an identity elsewhere (a
    /// vendor SDK may expose one the bus never sees).
    #[serde(default)]
    pub serial: Option<String>,
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
    /// Present COM port names (Windows).
    #[serde(default)]
    pub com_ports: Vec<String>,
    /// The host's USB inventory. Empty means an idle bus **only** when
    /// [`Self::usb_unavailable`] is `None`.
    #[serde(default)]
    pub usb: Vec<UsbDevice>,
    /// Why the USB scan could not be trusted, when it could not. `Some`
    /// makes [`Self::usb`] meaningless rather than empty: a collector that
    /// failed, timed out, or could not parse a candidate device record has
    /// no opinion about what is on the bus.
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
    /// The bus as staged. Every device carries a port, because a gathered
    /// candidate without one is an inventory failure, not a device.
    Devices(Vec<UsbDevice>),
    /// A scan that failed, carrying the reason a collector would have given.
    Unavailable(String),
}

/// The wire shape of a staged inventory: the two inventory fields of
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
    #[serde(default)]
    usb: Option<Vec<UsbDevice>>,
    #[serde(default)]
    usb_unavailable: Option<String>,
}

#[cfg(feature = "mock")]
impl TryFrom<StagedDocument> for StagedUsbInventory {
    type Error = String;

    fn try_from(document: StagedDocument) -> Result<Self, Self::Error> {
        // A document naming neither key states nothing, and the whole point
        // of this type is that nothing and empty are different answers. An
        // empty bus stays expressible, but has to be said out loud.
        if document.usb.is_none() && document.usb_unavailable.is_none() {
            return Err(
                "states neither a device list nor a failure; write `\"usb\": []` for an \
                 empty bus, or `usb_unavailable` with a reason for a failed scan"
                    .to_string(),
            );
        }
        let usb = document.usb.unwrap_or_default();
        match document.usb_unavailable {
            // A failed scan has no opinion about what is on the bus, so the
            // gatherer pairs the marker with an empty list. A document
            // claiming both would let a scenario assert on devices that a
            // failed scan could never have reported.
            Some(_) if !usb.is_empty() => Err(
                "names both a failure and a device list; a failed scan reports no devices"
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
                    let identity = format!("{}:{}", device.vendor, device.product);
                    // Blank is not the same absence as `None`, and it is the
                    // more dangerous one: an empty field matches nothing and
                    // reads like a device that simply did not match. No
                    // collector emits one — a candidate is a candidate
                    // because it has a vendor id, #1306 made an unreadable
                    // product fail the scan, and every port spelling has at
                    // least one component.
                    if device.vendor.trim().is_empty() || device.product.trim().is_empty() {
                        return Err(format!(
                            "device {identity} is missing a vendor or product id; a collector \
                             reports both for every candidate or fails the scan"
                        ));
                    }
                    if device.port.as_deref().is_none_or(|p| p.trim().is_empty()) {
                        return Err(format!(
                            "device {identity} has no port; a candidate without one is an \
                             inventory failure, so stage `usb_unavailable` to get that outcome"
                        ));
                    }
                    // Every collector stores what the platform reported
                    // with no padding around it: the sysfs read is trimmed,
                    // each `LocationPaths` element is trimmed before the
                    // `PCIROOT(` one is selected, and a macOS location id is
                    // a single whitespace-split token. So a padded value is
                    // unreachable — and it compares unequal to the same value
                    // without the padding, which is precisely the silent
                    // no-match the port key exists to rule out. Rejected
                    // rather than trimmed: silently rewriting a document
                    // hides the mistake instead of reporting it.
                    for (field, value) in [
                        ("vendor", Some(device.vendor.as_str())),
                        ("product", Some(device.product.as_str())),
                        ("port", device.port.as_deref()),
                        ("model", device.model.as_deref()),
                        ("serial", device.serial.as_deref()),
                    ] {
                        if let Some(value) = value {
                            if value != value.trim() {
                                return Err(format!(
                                    "device {identity} has a padded `{field}` ({value:?}); a \
                                     collector reports no padding, and a padded value compares \
                                     unequal to the same one without it"
                                ));
                            }
                        }
                    }
                }
                Ok(Self::Devices(usb))
            }
        }
    }
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
    fn into_scan(self) -> Result<Vec<UsbDevice>, String> {
        match self {
            Self::Devices(devices) => Ok(devices),
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

/// Gather hardware facts from the running host, read-only. Probe failures
/// degrade to absence with a `debug!` trail — "not there" is a legitimate
/// answer, not an error.
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
        facts.com_ports = windows::com_ports();
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
fn host_usb_scan() -> Result<Vec<UsbDevice>, String> {
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
        Ok(Vec::new())
    }
}

/// Land a collector's result on the facts, keeping "the scan failed"
/// distinct from "the bus is empty". On failure the inventory is left
/// empty *and* marked unavailable, so a consumer that ignores the marker
/// gets no devices rather than a plausible-looking partial list.
fn record_usb(facts: &mut HardwareFacts, scan: Result<Vec<UsbDevice>, String>) {
    match scan {
        Ok(devices) => facts.usb = devices,
        Err(reason) => {
            debug!(%reason, "USB inventory unavailable");
            facts.usb.clear();
            facts.usb_unavailable = Some(reason);
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

    use super::UsbDevice;

    /// Walk sysfs USB devices. An entry with an `idVendor` is a **candidate
    /// device record**; interfaces and root hubs have none and are skipped
    /// silently, as they always were. Once an entry is a candidate,
    /// anything unreadable about it fails the whole scan rather than
    /// yielding a partial record — a candidate with no port is
    /// indistinguishable from one whose port did not match a claim.
    ///
    /// The entry's own directory name *is* the port path: `1-4.2` reads as
    /// bus 1, root port 4, hub port 2.
    pub fn usb_inventory(devices_dir: &Path) -> Result<Vec<UsbDevice>, String> {
        let entries = std::fs::read_dir(devices_dir).map_err(|e| {
            debug!(path = %devices_dir.display(), error = %e, "sysfs USB walk failed");
            format!("sysfs USB walk failed at {}: {e}", devices_dir.display())
        })?;
        let mut inventory: Vec<UsbDevice> = Vec::new();
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
            let port = dir
                .file_name()
                .and_then(|name| name.to_str())
                .ok_or_else(|| {
                    format!(
                        "sysfs USB entry {} has no readable port path",
                        dir.display()
                    )
                })?;
            // `idProduct` is mandatory in the device descriptor, so a
            // candidate missing it is an unreadable entry rather than a
            // device without one. Defaulting it to empty would leave a
            // plausible-looking record that no VID:PID match can hit —
            // the "scan succeeded, device absent" answer this whole
            // distinction exists to prevent. `model` and `serial` are
            // genuinely optional and stay that way.
            let product = read_attr(&dir, "idProduct").ok_or_else(|| {
                format!(
                    "sysfs USB entry {} declares a vendor but no readable idProduct",
                    dir.display()
                )
            })?;
            inventory.push(UsbDevice {
                vendor,
                product,
                model: read_attr(&dir, "product"),
                port: Some(port.to_string()),
                serial: read_attr(&dir, "serial"),
            });
        }
        inventory.sort_by(|a, b| {
            (&a.vendor, &a.product, &a.port).cmp(&(&b.vendor, &b.product, &b.port))
        });
        Ok(inventory)
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

#[cfg(target_os = "macos")]
mod macos {
    use std::process::Command;

    use tracing::debug;

    use super::UsbDevice;

    /// `system_profiler -json SPUSBDataType`: hubs nest their devices
    /// under `_items`, so the walk recurses.
    pub fn usb_inventory() -> Result<Vec<UsbDevice>, String> {
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
        let mut inventory = Vec::new();
        if let Some(top) = value.get("SPUSBDataType").and_then(|v| v.as_array()) {
            for item in top {
                walk(item, &mut inventory)?;
            }
        }
        Ok(inventory)
    }

    fn walk(item: &serde_json::Value, inventory: &mut Vec<UsbDevice>) -> Result<(), String> {
        // A candidate device record is one presenting a vendor id; the
        // tree also carries controllers and other non-device nodes, which
        // are skipped silently as they always were.
        if let (Some(vendor), Some(product)) = (
            item.get("vendor_id").and_then(hex_field),
            item.get("product_id").and_then(hex_field),
        ) {
            let port = item
                .get("location_id")
                .and_then(location_id)
                .ok_or_else(|| {
                    format!("macOS USB device {vendor}:{product} reports no usable location_id")
                })?;
            inventory.push(UsbDevice {
                vendor,
                product,
                model: item
                    .get("_name")
                    .and_then(|v| v.as_str())
                    .map(str::to_string),
                port: Some(port),
                serial: item
                    .get("serial_num")
                    .and_then(|v| v.as_str())
                    .map(str::trim)
                    .filter(|s| !s.is_empty())
                    .map(str::to_string),
            });
        }
        if let Some(children) = item.get("_items").and_then(|v| v.as_array()) {
            for child in children {
                walk(child, inventory)?;
            }
        }
        Ok(())
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
    fn hex_field(value: &serde_json::Value) -> Option<String> {
        let text = value.as_str()?;
        let hex = text.strip_prefix("0x")?;
        let hex: String = hex.chars().take_while(char::is_ascii_hexdigit).collect();
        (hex.len() == 4).then(|| hex.to_lowercase())
    }
}

/// Gated on `test` as well as `windows` so the **pure parsers** below —
/// the location-path selection and the serial heuristic, which is where
/// the judgement lives — are exercised by every platform's CI leg rather
/// than only the Windows one. The impure entry points stay Windows-only.
#[cfg(any(windows, test))]
mod windows {
    #[cfg(windows)]
    use std::process::Command;

    #[cfg(windows)]
    use tracing::debug;

    use super::UsbDevice;

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

    #[cfg(windows)]
    pub fn com_ports() -> Vec<String> {
        powershell("[System.IO.Ports.SerialPort]::GetPortNames() -join \"`n\"")
            .ok()
            .map(|listing| {
                listing
                    .lines()
                    .map(str::trim)
                    .filter(|l| !l.is_empty())
                    .map(str::to_string)
                    .collect()
            })
            .unwrap_or_default()
    }

    /// Present USB devices from `PnP`: the instance id carries
    /// `USB\VID_xxxx&PID_xxxx\...`; the bus-reported device description is
    /// the product string the device itself sent; the location path is the
    /// port chain.
    #[cfg(windows)]
    pub fn usb_inventory() -> Result<Vec<UsbDevice>, String> {
        let script = "Get-PnpDevice -PresentOnly -ErrorAction SilentlyContinue | \
             Where-Object { $_.InstanceId -like 'USB\\VID_*' } | \
             ForEach-Object { \
                 $desc = (Get-PnpDeviceProperty -InstanceId $_.InstanceId \
                     -KeyName DEVPKEY_Device_BusReportedDeviceDesc \
                     -ErrorAction SilentlyContinue).Data; \
                 $paths = (Get-PnpDeviceProperty -InstanceId $_.InstanceId \
                     -KeyName DEVPKEY_Device_LocationPaths \
                     -ErrorAction SilentlyContinue).Data; \
                 \"$($_.InstanceId)`t$desc`t$($paths -join '|')\" }";
        let listing =
            powershell(script).map_err(|e| format!("Windows USB inventory failed: {e}"))?;
        parse_pnp_listing(&listing)
    }

    pub fn parse_pnp_listing(listing: &str) -> Result<Vec<UsbDevice>, String> {
        let mut inventory = Vec::new();
        for line in listing.lines() {
            let line = line.trim_end_matches('\r');
            if line.trim().is_empty() {
                continue;
            }
            let mut fields = line.splitn(3, '\t');
            let instance = fields.next().unwrap_or_default();
            let desc = fields.next().unwrap_or_default();
            let paths = fields.next().unwrap_or_default();

            // A candidate device record is a `USB\VID_…` instance id.
            let Some(rest) = instance.strip_prefix("USB\\VID_") else {
                continue;
            };
            let (Some(vendor), Some(product)) = (
                rest.get(..4).map(str::to_lowercase),
                rest.get(4..)
                    .and_then(|r| r.strip_prefix("&PID_"))
                    .and_then(|r| r.get(..4))
                    .map(str::to_lowercase),
            ) else {
                return Err(format!("Windows USB instance id {instance:?} is malformed"));
            };
            let port = location_path(paths).ok_or_else(|| {
                format!("Windows USB device {instance:?} reports no PCIROOT location path")
            })?;
            let model = desc.trim();
            inventory.push(UsbDevice {
                vendor,
                product,
                model: (!model.is_empty()).then(|| model.to_string()),
                port: Some(port),
                serial: instance_serial(instance),
            });
        }
        Ok(inventory)
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
        let inventory = linux::usb_inventory(&devices).unwrap();
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

        // A candidate that declares a vendor but whose product cannot be
        // read fails the scan rather than yielding an empty product that
        // no VID:PID match could ever hit.
        let half_read = devices.join("1-9");
        std::fs::create_dir_all(&half_read).unwrap();
        std::fs::write(half_read.join("idVendor"), "03c3\n").unwrap();
        let error = linux::usb_inventory(&devices)
            .expect_err("a candidate with no readable idProduct fails the scan");
        assert!(
            error.contains("idProduct"),
            "the error should name what was missing: {error}"
        );
        std::fs::remove_dir_all(&half_read).unwrap();

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

    /// Lines as the collector's PowerShell emits them: instance id, the
    /// bus-reported description, and the joined location paths. The values
    /// are real observations from the Starfront Windows rig.
    #[test]
    fn test_pnp_listing_parses_vid_pid_model_port_and_serial() {
        let listing = "USB\\VID_0403&PID_6015\\UPB248E11M\tUSB Serial Converter\t\
                       PCIROOT(0)#PCI(1400)#USBROOT(0)#USB(2)#USB(7)|\
                       ACPI(_SB_)#ACPI(PC00)#ACPI(XHCI)#ACPI(RHUB)#ACPI(HS02)#USB(7)\n\
                       USB\\VID_1618&PID_C601\\6&4213695&0&1\tQHY5IIISeries_IO\t\
                       PCIROOT(0)#PCI(1400)#USBROOT(0)#USB(14)#USB(1)|\
                       ACPI(_SB_)#ACPI(PC00)#ACPI(XHCI)#ACPI(RHUB)#ACPI(SS02)#USB(1)\n\
                       USB\\ROOT_HUB30\\4&1\tHub\tPCIROOT(0)#PCI(1400)#USBROOT(0)\n";
        let devices = super::windows::parse_pnp_listing(listing).unwrap();
        assert_eq!(devices.len(), 2, "a root hub is not a VID_ candidate");

        assert_eq!(devices[0].vendor, "0403");
        assert_eq!(devices[0].product, "6015");
        assert_eq!(devices[0].model.as_deref(), Some("USB Serial Converter"));
        assert_eq!(
            devices[0].port.as_deref(),
            Some("PCIROOT(0)#PCI(1400)#USBROOT(0)#USB(2)#USB(7)"),
            "the PCIROOT chain is the port, not the ACPI spelling beside it"
        );
        assert_eq!(
            devices[0].serial.as_deref(),
            Some("UPB248E11M"),
            "a device that published a serial keeps it in the instance id"
        );

        assert_eq!(
            devices[1].vendor, "1618",
            "instance-id hex normalizes to lowercase"
        );
        assert_eq!(
            devices[1].port.as_deref(),
            Some("PCIROOT(0)#PCI(1400)#USBROOT(0)#USB(14)#USB(1)")
        );
        assert_eq!(
            devices[1].serial, None,
            "a synthesized parent-relative id encodes the port, not a serial"
        );
    }

    #[test]
    fn test_location_path_selection_rejects_an_acpi_only_device() {
        // Observed on the same rig: a device whose descriptor request
        // failed publishes the ACPI spelling alone. It is a candidate with
        // no usable port, which fails the scan rather than yielding a
        // record the claims resolver cannot place.
        let listing = "USB\\VID_0000&PID_0002\\5&27E528BF&0&5\tUnknown\t\
                       ACPI(_SB_)#ACPI(PC00)#ACPI(XHCI)#ACPI(RHUB)#ACPI(HS05)\n";
        let error = super::windows::parse_pnp_listing(listing)
            .expect_err("an ACPI-only device has no usable port path");
        assert!(
            error.contains("PCIROOT"),
            "the error should name what was missing: {error}"
        );
    }

    #[test]
    fn test_pnp_listing_rejects_a_malformed_instance_id() {
        let listing = "USB\\VID_ZZ\tBroken\tPCIROOT(0)#PCI(1400)#USBROOT(0)#USB(1)\n";
        super::windows::parse_pnp_listing(listing)
            .expect_err("a candidate whose vendor cannot be read fails the scan");
    }

    /// The staged USB inventory — docs/services/doctor.md, "USB inventory".
    #[cfg(feature = "mock")]
    mod staged_inventory {
        use std::path::{Path, PathBuf};

        use super::super::{gather, ProbeRequest, StagedUsbInventory};

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

        /// Replaces the scan rather than adding to it: the gathered bus is
        /// exactly what was staged, on a dev box whose own bus is not.
        #[test]
        fn test_a_staged_device_list_replaces_the_host_scan() {
            let dir = tempfile::tempdir().unwrap();
            let path = stage(
                dir.path(),
                r#"{ "usb": [ { "vendor": "1618", "product": "c601",
                     "model": "QHY5IIISeries_IO",
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
            assert!(facts.usb.is_empty());
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

        /// A gathered candidate without a port is an inventory failure, so
        /// staging one would let a scenario assert on a state the runtime
        /// rejects. The message says what to stage instead.
        #[test]
        fn test_a_staged_device_without_a_port_is_rejected() {
            let dir = tempfile::tempdir().unwrap();
            let path = stage(
                dir.path(),
                r#"{ "usb": [ { "vendor": "1618", "product": "c601" } ] }"#,
            );
            let error = StagedUsbInventory::load(&path).unwrap_err();
            assert!(error.contains("1618:c601"), "{error}");
            assert!(error.contains("usb_unavailable"), "{error}");
        }

        /// An empty bus is a state every collector can report, so it stays
        /// stageable — `Ok(empty)` means a genuinely idle bus, which is how
        /// a claimed port with nothing in it gets exercised.
        #[test]
        fn test_an_explicitly_empty_bus_is_an_empty_bus() {
            let dir = tempfile::tempdir().unwrap();
            let path = stage(dir.path(), r#"{ "usb": [] }"#);
            let facts = gather(&request(StagedUsbInventory::load(&path).unwrap()));
            assert!(facts.usb.is_empty());
            assert!(facts.usb_unavailable.is_none());
            assert_eq!(facts.usb_present("1618", None, None), Some(false));
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
        /// collector reports it for every candidate or fails the scan.
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
                r#"{ "paths": {}, "com_ports": [], "groups": { "plugdev": 46 },
                     "udev_rules": {},
                     "usb": [ { "vendor": "03c3", "product": "662b",
                                "model": "ASI662MC", "port": "1-4.2",
                                "serial": null } ] }"#,
            );
            let staged = StagedUsbInventory::load(&path).unwrap();
            assert_eq!(
                staged,
                StagedUsbInventory::Devices(vec![super::super::UsbDevice {
                    vendor: "03c3".to_string(),
                    product: "662b".to_string(),
                    model: Some("ASI662MC".to_string()),
                    port: Some("1-4.2".to_string()),
                    serial: None,
                }])
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
}
