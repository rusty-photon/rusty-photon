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
    /// The host's USB inventory.
    #[serde(default)]
    pub usb: Vec<UsbDevice>,
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
    #[must_use]
    pub fn usb_present(&self, vendor: &str, product: Option<&str>, model: Option<&str>) -> bool {
        self.usb.iter().any(|d| {
            d.vendor == vendor
                && product.is_none_or(|p| d.product == p)
                && model.is_none_or(|m| d.model.as_deref().is_some_and(|dm| dm.contains(m)))
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
        facts.usb = linux::usb_inventory(Path::new("/sys/bus/usb/devices"));
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
    #[cfg(target_os = "macos")]
    {
        facts.usb = macos::usb_inventory();
    }
    #[cfg(windows)]
    {
        facts.com_ports = windows::com_ports();
        facts.usb = windows::usb_inventory();
    }
    facts
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

    /// Walk sysfs USB devices: every entry with an `idVendor` is a device
    /// (interfaces have none).
    pub fn usb_inventory(devices_dir: &Path) -> Vec<UsbDevice> {
        let entries = match std::fs::read_dir(devices_dir) {
            Ok(entries) => entries,
            Err(e) => {
                debug!(path = %devices_dir.display(), error = %e, "sysfs USB walk failed");
                return Vec::new();
            }
        };
        let mut inventory: Vec<UsbDevice> = entries
            .filter_map(Result::ok)
            .filter_map(|entry| {
                let dir = entry.path();
                let vendor = read_attr(&dir, "idVendor")?;
                let product = read_attr(&dir, "idProduct").unwrap_or_default();
                let model = read_attr(&dir, "product");
                Some(UsbDevice {
                    vendor,
                    product,
                    model,
                })
            })
            .collect();
        inventory.sort_by(|a, b| (&a.vendor, &a.product).cmp(&(&b.vendor, &b.product)));
        inventory
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

#[cfg(target_os = "macos")]
mod macos {
    use std::process::Command;

    use tracing::debug;

    use super::UsbDevice;

    /// `system_profiler -json SPUSBDataType`: hubs nest their devices
    /// under `_items`, so the walk recurses.
    pub fn usb_inventory() -> Vec<UsbDevice> {
        let output = match Command::new("system_profiler")
            .args(["-json", "SPUSBDataType"])
            .output()
        {
            Ok(output) if output.status.success() => output.stdout,
            other => {
                debug!(result = ?other.map(|o| o.status), "system_profiler query failed");
                return Vec::new();
            }
        };
        let Ok(value) = serde_json::from_slice::<serde_json::Value>(&output) else {
            debug!("system_profiler output is not valid JSON");
            return Vec::new();
        };
        let mut inventory = Vec::new();
        if let Some(top) = value.get("SPUSBDataType").and_then(|v| v.as_array()) {
            for item in top {
                walk(item, &mut inventory);
            }
        }
        inventory
    }

    fn walk(item: &serde_json::Value, inventory: &mut Vec<UsbDevice>) {
        if let (Some(vendor), Some(product)) = (
            item.get("vendor_id").and_then(hex_field),
            item.get("product_id").and_then(hex_field),
        ) {
            inventory.push(UsbDevice {
                vendor,
                product,
                model: item
                    .get("_name")
                    .and_then(|v| v.as_str())
                    .map(str::to_string),
            });
        }
        if let Some(children) = item.get("_items").and_then(|v| v.as_array()) {
            for child in children {
                walk(child, inventory);
            }
        }
    }

    /// `vendor_id` renders as `0x0403` or `0x0403  (Vendor Name)`.
    fn hex_field(value: &serde_json::Value) -> Option<String> {
        let text = value.as_str()?;
        let hex = text.strip_prefix("0x")?;
        let hex: String = hex.chars().take_while(char::is_ascii_hexdigit).collect();
        (hex.len() == 4).then(|| hex.to_lowercase())
    }
}

#[cfg(windows)]
mod windows {
    use std::process::Command;

    use tracing::debug;

    use super::UsbDevice;

    fn powershell(script: &str) -> Option<String> {
        match Command::new("powershell.exe")
            .args(["-NoProfile", "-NonInteractive", "-Command", script])
            .output()
        {
            Ok(output) if output.status.success() => {
                Some(String::from_utf8_lossy(&output.stdout).into_owned())
            }
            other => {
                debug!(result = ?other.map(|o| o.status), "powershell query failed");
                None
            }
        }
    }

    pub fn com_ports() -> Vec<String> {
        powershell("[System.IO.Ports.SerialPort]::GetPortNames() -join \"`n\"")
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
    /// the product string the device itself sent.
    pub fn usb_inventory() -> Vec<UsbDevice> {
        let script = "Get-PnpDevice -PresentOnly -ErrorAction SilentlyContinue | \
             Where-Object { $_.InstanceId -like 'USB\\VID_*' } | \
             ForEach-Object { \
                 $desc = (Get-PnpDeviceProperty -InstanceId $_.InstanceId \
                     -KeyName DEVPKEY_Device_BusReportedDeviceDesc \
                     -ErrorAction SilentlyContinue).Data; \
                 \"$($_.InstanceId)`t$desc\" }";
        powershell(script)
            .map(|listing| parse_pnp_listing(&listing))
            .unwrap_or_default()
    }

    pub fn parse_pnp_listing(listing: &str) -> Vec<UsbDevice> {
        listing
            .lines()
            .filter_map(|line| {
                let (instance, desc) = line.split_once('\t').unwrap_or((line, ""));
                let rest = instance.strip_prefix("USB\\VID_")?;
                let vendor = rest.get(..4)?.to_lowercase();
                let product = rest
                    .get(4..)?
                    .strip_prefix("&PID_")?
                    .get(..4)?
                    .to_lowercase();
                let model = desc.trim();
                Some(UsbDevice {
                    vendor,
                    product,
                    model: (!model.is_empty()).then(|| model.to_string()),
                })
            })
            .collect()
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
        assert!(facts.usb.is_empty());
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
        assert!(facts.usb_present("0403", None, None));
        assert!(facts.usb_present("0403", Some("6015"), Some("Falcon")));
        assert!(
            !facts.usb_present("0403", Some("6015"), Some("PPBA")),
            "the model substring must discriminate devices sharing a bridge VID:PID"
        );
        assert!(!facts.usb_present("0403", Some("6001"), None));
        assert!(
            !facts.usb_present("1618", None, Some("Q-Focuser")),
            "a declared model never matches a device that reports none"
        );
        assert!(!facts.usb_present("03c3", None, None));
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
        assert!(unix::user_groups(&dir.path().join("absent"), "rusty-photon").is_empty());

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
        for (entry, vendor, product, model) in [
            ("1-1", Some("0403"), Some("6015"), Some("Falcon Rotator")),
            ("1-2", Some("1618"), Some("c179"), None),
            ("1-1:1.0", None, None, None), // an interface — no idVendor
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
        }
        let inventory = linux::usb_inventory(&devices);
        assert_eq!(inventory.len(), 2, "interfaces are not devices");
        assert_eq!(inventory[0].vendor, "0403");
        assert_eq!(inventory[0].model.as_deref(), Some("Falcon Rotator"));
        assert_eq!(inventory[1].vendor, "1618");
        assert_eq!(inventory[1].model, None);

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

    #[cfg(windows)]
    #[test]
    fn test_pnp_listing_parses_vid_pid_and_model() {
        let listing = "USB\\VID_0403&PID_6015\\PPBAAYSK3N\tPPBADV Gen2C\n\
                       USB\\VID_2E8A&PID_000A\\E46\t\n\
                       USB\\ROOT_HUB30\\4&1\tHub\n";
        let devices = super::windows::parse_pnp_listing(listing);
        assert_eq!(devices.len(), 2);
        assert_eq!(devices[0].vendor, "0403");
        assert_eq!(devices[0].product, "6015");
        assert_eq!(devices[0].model.as_deref(), Some("PPBADV Gen2C"));
        assert_eq!(
            devices[1].vendor, "2e8a",
            "registry hex normalizes to lowercase"
        );
        assert_eq!(devices[1].model, None);
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
