//! The packaging files agree with each other, and each package carries what
//! it needs.
//!
//! A packaged service is a `services/*` workspace member with a `pkg/`
//! directory or a `[package.metadata.deb]` table. Its .deb and .rpm are built
//! from that table and `pkg/`, its macOS tarball and Homebrew formula by
//! `scripts/build-tarballs.sh` and `cargo ci brew-formulas`, and
//! its feature of the Windows suite MSI from
//! `installer/fragments/<service>.wxs` (docs/packaging.md, ADR-012,
//! ADR-015). Most of what can go wrong there shows only when a nightly
//! installs the package, long after the pull request that broke it merged.
//! These tests catch it in that pull request:
//!
//! - **Units.** Each systemd unit starts `/usr/bin/rusty-photon-<service>`
//!   with no arguments: the config is found by XDG. A service that reloads
//!   on SIGHUP ([`RELOADING`]) has the `ExecReload` that sends it, and one
//!   with no config it can default ([`GATED`]) waits for its config with
//!   `ConditionPathExists`.
//! - **Groups.** A serial driver ([`SERIAL`]) keeps a Debian unit with
//!   `SupplementaryGroups=dialout plugdev` and an rpm unit with `dialout`
//!   alone, the same otherwise, and its `Cargo.toml` ships each flavor's. A
//!   camera-class package ([`CAMERAS`]) gives its devices to the
//!   `rusty-photon` group in its udev rules, never plugdev, and its unit
//!   adds no group. No rpm scriptlet creates plugdev, a Debian group, and
//!   no rpm unit names it.
//! - **Maintainer scripts.** `pkg/postrm` is `packaging/postrm.common`.
//!   `pkg/postinst` is `packaging/postinst.common`, with
//!   `packaging/postinst.udev-stanza` before `#DEBHELPER#` for
//!   [`UDEV_STANZA`]'s packages. svbony-camera's keeps the common preamble
//!   and spells out its own udev reload and the pointer to its SDK helper.
//! - **Each package's own files** ([`OWN_FILES`]): its udev or polkit rule;
//!   sentinel's doctor binary and TLS renewal units, in both flavors;
//!   svbony-camera's SDK helper, which installs the SDK into the RUNPATH
//!   `scripts/build-packages.sh` links with and writes a development udev
//!   rule of its own, for the packaged rule's vendor and never world-writable.
//! - **Names.** The deb, its systemd unit and the rpm are all
//!   `rusty-photon-<service>`.
//! - **SDK pins**, where they are copied: the QHY SDK's version and
//!   sha256s, the ZWO and `SVBony` refs and the ZWO Windows URLs agree across
//!   the packaging scripts, the shipped helpers, the install actions,
//!   `libqhyccd-sys`'s `build.rs` and qhy-camera's `PINNED_SDK_VERSION`;
//!   the zwo packages' license is the one vendored with the SDK headers.
//! - **macOS.** `build-tarballs.sh` leaves out svbony-camera, the service
//!   `cargo ci brew-formulas` renders no formula for: it has no macOS SDK,
//!   so no tarball for a formula to point at.
//! - **Windows.** Every packaged service but svbony-camera has a fragment
//!   that installs it as a Windows service of its package's name, from its
//!   exe, with `--service`, restarting after 5 s with the failure-actions
//!   flag, its firewall port as `verify-packages.sh` gives it, and starts it
//!   on install, or installs it demand-start when it is [`GATED`].
//!   `Package.wxs` wires each feature, every fragment belongs to a service,
//!   and doctor rides in sentinel's fragment with its renewal task.
//! - **Port tables.** `verify-packages.sh` and `verify-brew.sh` each give
//!   every packaged service a port (the brew table all but svbony-camera),
//!   and they agree.
//!
//! This is the one list of these checks; the packaging docs point here. One
//! run names every problem it finds.
//!
//! The units, scripts and `WiX` fragments are read line by line, as the shell
//! they replaced did; `Cargo.toml` with the TOML parser. Under Bazel the
//! files are the unit test's data in `BUILD.bazel`, so a file missing from it
//! fails these tests: every `services/*` member must be in its `_SERVICES`.

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::{Path, PathBuf};

use toml::{Table, Value};

use crate::repo_root;

/// Serial drivers. Debian puts USB serial adapters in plugdev, which
/// rpm-family hosts do not have, so each keeps a unit per flavor.
const SERIAL: &[&str] = &[
    "ppba-driver",
    "upbv2-driver",
    "qhy-focuser",
    "pa-falcon-rotator",
    "pa-scops-oag",
    "dsd-fp2",
    "star-adventurer-gti",
];

/// Services that reload their config on SIGHUP.
const RELOADING: &[&str] = &[
    "filemonitor",
    "ppba-driver",
    "upbv2-driver",
    "qhy-focuser",
    "sky-survey-camera",
    "pa-falcon-rotator",
    "pa-scops-oag",
    "dsd-fp2",
    "star-adventurer-gti",
    "qhy-camera",
    "zwo-camera",
    "zwo-focuser",
    "svbony-camera",
];

/// Services with no config they can default. Their units wait for one, and
/// the MSI installs them demand-start, where a fresh install would otherwise
/// start a service that can only fail.
const GATED: &[&str] = &[
    "sky-survey-camera",
    "plate-solver",
    "calibrator-flats",
    "focus-model",
    "session-runner",
    "polar-align",
];

/// Camera-class packages: their udev rules give the devices to the service's
/// own group, so the unit needs no other.
const CAMERAS: &[&str] = &["qhy-camera", "zwo-camera", "zwo-focuser", "svbony-camera"];

/// The packages whose postinst adds the udev stanza to the common one.
/// svbony-camera ships a udev rule too, but its SDK helper is not named as
/// the stanza derives it, so its postinst spells that part out.
const UDEV_STANZA: &[&str] = &["qhy-camera", "zwo-camera", "zwo-focuser"];

/// A file each of these packages carries in `pkg/`.
const OWN_FILES: &[(&str, &str)] = &[
    // The polkit rule that lets sentinel restart the other services.
    ("sentinel", "50-rusty-photon-sentinel.rules"),
    ("qhy-camera", "90-rusty-photon-qhy.rules"),
    ("zwo-camera", "90-rusty-photon-zwo.rules"),
    // Not zwo-camera's name, so both rules install on one host.
    ("zwo-focuser", "90-rusty-photon-zwo-focuser.rules"),
    ("svbony-camera", "90-rusty-photon-svbony.rules"),
];

/// It has no `SVBony` SDK on macOS or Windows (ADR-018), so no tarball,
/// formula or MSI feature.
const SVBONY: &str = "svbony-camera";

const BUILD_PACKAGES: &str = "scripts/build-packages.sh";
const BUILD_TARBALLS: &str = "scripts/build-tarballs.sh";
const BUILD_MSI: &str = "scripts/build-msi.ps1";
const VERIFY_PACKAGES: &str = "scripts/verify-packages.sh";
const VERIFY_BREW: &str = "scripts/verify-brew.sh";
const ZWO_MODULE: &str = ".github/actions/install-zwo-sdk/zwo-sdk.mjs";
const SVBONY_MODULE: &str = ".github/actions/install-svbony-sdk/svbony-sdk.mjs";
const QHY_HELPER: &str = "services/qhy-camera/pkg/rusty-photon-qhy-firmware-install";
const SVBONY_HELPER: &str = "services/svbony-camera/pkg/rusty-photon-svbony-sdk-install";
const QHY_BUILD_RS: &str = "crates/qhyccd-rs/libqhyccd-sys/build.rs";
const QHY_PREFLIGHT: &str = "services/qhy-camera/src/preflight.rs";
const ZWO_LICENSE: &str = "crates/zwo-rs/libzwo-sys/sdk/include/license.txt";
const POSTINST: &str = "packaging/postinst.common";
const POSTRM: &str = "packaging/postrm.common";
const UDEV: &str = "packaging/postinst.udev-stanza";
const PACKAGE_WXS: &str = "installer/Package.wxs";
const FRAGMENTS: &str = "installer/fragments";

/// The repository's files, with a test's changes laid over them.
struct Repo {
    root: PathBuf,
    /// A path's text as a test changed it, or `None` where it removed the file.
    changes: BTreeMap<String, Option<String>>,
}

impl Repo {
    fn new() -> Self {
        Self {
            root: repo_root(),
            changes: BTreeMap::new(),
        }
    }

    fn read(&self, path: &str) -> Result<String, String> {
        match self.changes.get(path) {
            Some(Some(text)) => Ok(text.clone()),
            Some(None) => Err("removed".to_owned()),
            None => fs::read_to_string(self.root.join(path)).map_err(|error| error.to_string()),
        }
    }

    fn is_dir(&self, dir: &str) -> bool {
        self.root.join(dir).is_dir()
            || self.changes.iter().any(|(path, text)| {
                text.is_some()
                    && path
                        .strip_prefix(dir)
                        .is_some_and(|rest| rest.starts_with('/'))
            })
    }

    /// The names of the files directly in `dir`.
    fn files(&self, dir: &str) -> BTreeSet<String> {
        let mut names: BTreeSet<String> = fs::read_dir(self.root.join(dir))
            .into_iter()
            .flatten()
            .flatten()
            .filter(|entry| entry.path().is_file())
            .filter_map(|entry| entry.file_name().into_string().ok())
            .collect();
        let changed = self.changes.iter().filter_map(|(path, text)| {
            let (parent, name) = path.rsplit_once('/')?;
            (parent == dir).then_some((name, text))
        });
        for (name, text) in changed {
            if text.is_some() {
                names.insert(name.to_owned());
            } else {
                names.remove(name);
            }
        }
        names
    }
}

/// A run's problems so far.
struct Check<'a> {
    repo: &'a Repo,
    problems: Vec<String>,
}

impl Check<'_> {
    /// Records `problem` unless `ok`.
    fn require(&mut self, ok: bool, problem: impl FnOnce() -> String) {
        if !ok {
            self.problems.push(problem());
        }
    }

    /// The text of `path`, or `None` once the problem is recorded.
    fn read(&mut self, path: &str) -> Option<String> {
        self.repo
            .read(path)
            .map_err(|error| {
                self.problems.push(format!(
                    "cannot read {path}: {error} (under Bazel, is it in the unit test's data?)"
                ));
            })
            .ok()
    }
}

/// A `services/*` workspace member.
struct Service {
    name: String,
    /// Its `Cargo.toml`, empty when it could not be read.
    text: String,
    /// Its `Cargo.toml`, when it parsed.
    manifest: Option<Table>,
    packaged: bool,
}

impl Service {
    /// `rusty-photon-<service>`, the name of everything it ships.
    fn package(&self) -> String {
        format!("rusty-photon-{}", self.name)
    }

    fn pkg(&self) -> String {
        format!("services/{}/pkg", self.name)
    }

    fn is(&self, class: &[&str]) -> bool {
        class.contains(&self.name.as_str())
    }

    fn deb(&self) -> Option<&Table> {
        table_at(self.manifest.as_ref()?, &["package", "metadata", "deb"])
    }

    fn rpm(&self) -> Option<&Table> {
        table_at(
            self.manifest.as_ref()?,
            &["package", "metadata", "generate-rpm"],
        )
    }

    /// `[package.metadata.deb]`'s `systemd-units`: one table, or several.
    fn deb_units(&self) -> Vec<&Table> {
        tables(self.deb().and_then(|deb| deb.get("systemd-units")))
    }

    /// Whether the deb's `assets` holds `[source, dest, mode]`.
    fn deb_ships(&self, asset: [&str; 3]) -> bool {
        self.deb()
            .and_then(|deb| deb.get("assets"))
            .and_then(Value::as_array)
            .is_some_and(|assets| {
                assets.iter().any(|entry| {
                    entry.as_array().is_some_and(|items| {
                        items
                            .iter()
                            .map(Value::as_str)
                            .eq(asset.into_iter().map(Some))
                    })
                })
            })
    }

    /// Whether one of the rpm's `assets` has `key` set to `value`.
    fn rpm_ships(&self, key: &str, value: &str) -> bool {
        tables(self.rpm().and_then(|rpm| rpm.get("assets")))
            .into_iter()
            .any(|asset| asset.get(key).and_then(Value::as_str) == Some(value))
    }
}

/// The table at `keys` in `table`.
fn table_at<'a>(table: &'a Table, keys: &[&str]) -> Option<&'a Table> {
    keys.iter()
        .try_fold(table, |table, key| table.get(*key)?.as_table())
}

/// A table, or each table of an array of them.
fn tables(value: Option<&Value>) -> Vec<&Table> {
    match value {
        Some(Value::Table(table)) => vec![table],
        Some(Value::Array(items)) => items.iter().filter_map(Value::as_table).collect(),
        _ => Vec::new(),
    }
}

fn string<'a>(table: Option<&'a Table>, key: &str) -> Option<&'a str> {
    table?.get(key)?.as_str()
}

fn has_line(text: &str, line: &str) -> bool {
    text.lines().any(|candidate| candidate == line)
}

fn has_line_starting(text: &str, prefix: &str) -> bool {
    text.lines().any(|line| line.starts_with(prefix))
}

/// What the first line `<before><value><after>` holds between the two.
fn value<'a>(text: &'a str, before: &str, after: &str) -> Option<&'a str> {
    text.lines()
        .find_map(|line| line.strip_prefix(before)?.strip_suffix(after))
}

/// A shell assignment's value: `NAME="<value>"`.
fn shell_var<'a>(text: &'a str, name: &str) -> Option<&'a str> {
    value(text, &format!("{name}=\""), "\"")
}

/// A module's one-line `export const NAME = '<value>';`.
fn js_const<'a>(text: &'a str, name: &str) -> Option<&'a str> {
    value(text, &format!("export const {name} = '"), "';")
}

/// A PowerShell assignment's value: `$Name = "<value>"`.
fn ps_var<'a>(text: &'a str, name: &str) -> Option<&'a str> {
    value(text, &format!("${name} = \""), "\"")
}

/// Records a problem unless the two copies of a pin are the same, and not
/// empty.
fn agree(
    check: &mut Check,
    pin: &str,
    (left_file, left): (&str, Option<&str>),
    (right_file, right): (&str, Option<&str>),
) {
    let left = left.unwrap_or_default();
    let right = right.unwrap_or_default();
    check.require(!left.is_empty() && left == right, || {
        format!("{pin} pin mismatch: {left_file}='{left}' vs {right_file}='{right}'")
    });
}

/// The workspace's `services/*` members.
fn services(check: &mut Check) -> Vec<Service> {
    let root = check.read("Cargo.toml").unwrap_or_default();
    let members: Vec<String> = root
        .parse::<Table>()
        .ok()
        .as_ref()
        .and_then(|root| table_at(root, &["workspace"]))
        .and_then(|workspace| workspace.get("members"))
        .and_then(Value::as_array)
        .map(|members| {
            members
                .iter()
                .filter_map(Value::as_str)
                .filter_map(|member| member.strip_prefix("services/"))
                .filter(|name| !name.contains('/'))
                .map(str::to_owned)
                .collect()
        })
        .unwrap_or_default();
    check.require(!members.is_empty(), || {
        "Cargo.toml: no services/* workspace members".to_owned()
    });
    members
        .into_iter()
        .map(|name| {
            let path = format!("services/{name}/Cargo.toml");
            let text = check.read(&path).unwrap_or_default();
            let manifest = text
                .parse::<Table>()
                .map_err(|error| {
                    if !text.is_empty() {
                        check.problems.push(format!("{path}: {error}"));
                    }
                })
                .ok();
            let has_deb = manifest
                .as_ref()
                .and_then(|manifest| table_at(manifest, &["package", "metadata", "deb"]))
                .is_some();
            let has_pkg = check.repo.is_dir(&format!("services/{name}/pkg"));
            check.require(has_pkg || !has_deb, || {
                format!(
                    "services/{name}: [package.metadata.deb] but no pkg/ directory \
                     (under Bazel, is //services/{name}:pkg in the unit test's data?)"
                )
            });
            Service {
                packaged: has_pkg || has_deb,
                name,
                text,
                manifest,
            }
        })
        .collect()
}

/// The files several checks read, each read once.
struct Shared {
    postinst: Option<String>,
    postrm: Option<String>,
    udev_stanza: Option<String>,
    build_packages: Option<String>,
    build_msi: Option<String>,
    zwo_module: Option<String>,
    svbony_module: Option<String>,
    verify_packages: Option<String>,
}

fn problems(repo: &Repo) -> Vec<String> {
    let mut check = Check {
        repo,
        problems: Vec::new(),
    };
    let services = services(&mut check);
    let shared = Shared {
        postinst: check.read(POSTINST),
        postrm: check.read(POSTRM),
        udev_stanza: check.read(UDEV),
        build_packages: check.read(BUILD_PACKAGES),
        build_msi: check.read(BUILD_MSI),
        zwo_module: check.read(ZWO_MODULE),
        svbony_module: check.read(SVBONY_MODULE),
        verify_packages: check.read(VERIFY_PACKAGES),
    };
    let packaged: Vec<&Service> = services.iter().filter(|service| service.packaged).collect();
    for service in &packaged {
        units(&mut check, service);
        groups(&mut check, service);
        maintainer_scripts(&mut check, service, &shared);
        own_files(&mut check, service, &shared);
        names(&mut check, service);
    }
    plugdev(&mut check, &services, &packaged);
    sdk_pins(&mut check, &shared);
    macos(&mut check, &shared);
    installer(&mut check, &packaged, &shared);
    port_tables(&mut check, &packaged, &shared);
    check.problems
}

/// The units a service ships: one, or a serial driver's two.
fn unit_paths(service: &Service) -> Vec<String> {
    let (pkg, package) = (service.pkg(), service.package());
    if service.is(SERIAL) {
        vec![
            format!("{pkg}/deb/{package}.service"),
            format!("{pkg}/rpm/{package}.service"),
        ]
    } else {
        vec![format!("{pkg}/{package}.service")]
    }
}

fn units(check: &mut Check, service: &Service) {
    let (svc, package) = (&service.name, service.package());
    for path in unit_paths(service) {
        let Some(unit) = check.read(&path) else {
            continue;
        };
        check.require(
            has_line(&unit, &format!("ExecStart=/usr/bin/{package}")),
            || {
                format!(
                    "{svc}: ExecStart must be exactly /usr/bin/{package} \
                     (config is XDG-resolved; no --config flag)"
                )
            },
        );
        if service.is(RELOADING) {
            check.require(has_line(&unit, "ExecReload=/bin/kill -HUP $MAINPID"), || {
                format!("{svc}: reload-capable service must have ExecReload=/bin/kill -HUP $MAINPID")
            });
        }
        if service.is(GATED) {
            let gate = format!(
                "ConditionPathExists=/var/lib/rusty-photon/.config/rusty-photon/{svc}.json"
            );
            check.require(has_line(&unit, &gate), || {
                format!(
                    "{svc}: no-default-config service must gate on \
                     ConditionPathExists=<XDG config path>"
                )
            });
        }
    }
}

/// The serial drivers' two units, and the camera-class packages' group.
fn groups(check: &mut Check, service: &Service) {
    let (svc, pkg, package) = (&service.name, service.pkg(), service.package());
    if service.is(SERIAL) {
        let deb = check.repo.read(&format!("{pkg}/deb/{package}.service"));
        let rpm = check.repo.read(&format!("{pkg}/rpm/{package}.service"));
        if let (Ok(deb), Ok(rpm)) = (deb, rpm) {
            check.require(
                has_line(&deb, "SupplementaryGroups=dialout plugdev"),
                || format!("{svc}: deb unit must carry SupplementaryGroups=dialout plugdev"),
            );
            check.require(has_line(&rpm, "SupplementaryGroups=dialout"), || {
                format!("{svc}: rpm unit must carry exactly SupplementaryGroups=dialout")
            });
            let others = |unit: &str| -> Vec<String> {
                unit.lines()
                    .filter(|line| !line.starts_with("SupplementaryGroups="))
                    .map(str::to_owned)
                    .collect()
            };
            check.require(others(&deb) == others(&rpm), || {
                format!("{svc}: deb and rpm units must differ only in the SupplementaryGroups line")
            });
        }
        let from_deb_dir = service
            .deb_units()
            .iter()
            .any(|unit| string(Some(unit), "unit-scripts") == Some("pkg/deb/"));
        check.require(from_deb_dir, || {
            format!("{svc}: deb systemd-units must take the unit from pkg/deb/")
        });
        check.require(
            service.rpm_ships("source", &format!("pkg/rpm/{package}.service")),
            || format!("{svc}: rpm assets must ship the pkg/rpm/ unit"),
        );
    }
    if service.is(CAMERAS) {
        let rules = check.repo.files(&pkg);
        let rules = rules.iter().filter(|name| {
            name.starts_with("90-")
                && Path::new(name)
                    .extension()
                    .is_some_and(|ext| ext == "rules")
        });
        for rule in rules {
            let Some(text) = check.read(&format!("{pkg}/{rule}")) else {
                continue;
            };
            check.require(text.contains("GROUP=\"rusty-photon\""), || {
                format!("{svc}: {rule} must assign GROUP=\"rusty-photon\"")
            });
            check.require(!text.contains("GROUP=\"plugdev\""), || {
                format!("{svc}: {rule} must not assign plugdev")
            });
        }
        if let Ok(unit) = check.repo.read(&format!("{pkg}/{package}.service")) {
            check.require(!has_line_starting(&unit, "SupplementaryGroups="), || {
                format!("{svc}: camera unit needs no SupplementaryGroups (nodes are rusty-photon-owned)")
            });
        }
    }
}

/// `common` with `stanza` before its first `#DEBHELPER#` line.
fn with_stanza(common: &str, stanza: &str) -> String {
    let mut text = String::new();
    let mut inserted = false;
    for line in common.lines() {
        if !inserted && line.starts_with("#DEBHELPER#") {
            for stanza_line in stanza.lines() {
                text.push_str(stanza_line);
                text.push('\n');
            }
            inserted = true;
        }
        text.push_str(line);
        text.push('\n');
    }
    text
}

/// The first `count` lines of `text`, with their line ends.
fn head(text: &str, count: usize) -> String {
    text.split_inclusive('\n').take(count).collect()
}

fn maintainer_scripts(check: &mut Check, service: &Service, shared: &Shared) {
    let (svc, pkg) = (&service.name, service.pkg());
    let (Some(common), Some(common_rm)) = (&shared.postinst, &shared.postrm) else {
        return;
    };
    if let Some(postrm) = check.read(&format!("{pkg}/postrm")) {
        check.require(&postrm == common_rm, || {
            format!("{svc}: pkg/postrm differs from {POSTRM}")
        });
    }
    let Some(postinst) = check.read(&format!("{pkg}/postinst")) else {
        return;
    };
    if service.is(UDEV_STANZA) {
        if let Some(stanza) = &shared.udev_stanza {
            check.require(postinst == with_stanza(common, stanza), || {
                format!(
                    "{svc}: pkg/postinst must be postinst.common + udev stanza before #DEBHELPER#"
                )
            });
        }
    } else if svc == SVBONY {
        // Everything but postinst.common's last line, `#DEBHELPER#`, which
        // this postinst has later.
        let preamble = common.matches('\n').count().saturating_sub(1);
        check.require(head(&postinst, preamble) == head(common, preamble), || {
            format!("{svc}: pkg/postinst's user/dir/symlink preamble must match {POSTINST}")
        });
        check.require(
            has_line(&postinst, "udevadm control --reload-rules || true"),
            || format!("{svc}: pkg/postinst must reload udev rules"),
        );
        check.require(has_line(&postinst, "udevadm trigger || true"), || {
            format!("{svc}: pkg/postinst must trigger udev")
        });
        check.require(
            postinst.contains("rusty-photon-svbony-sdk-install' once as root"),
            || format!("{svc}: pkg/postinst must point the operator at rusty-photon-svbony-sdk-install"),
        );
        check.require(has_line(&postinst, "#DEBHELPER#"), || {
            format!("{svc}: pkg/postinst must end with #DEBHELPER#")
        });
    } else {
        check.require(&postinst == common, || {
            format!("{svc}: pkg/postinst differs from {POSTINST}")
        });
    }
}

fn own_files(check: &mut Check, service: &Service, shared: &Shared) {
    let (svc, pkg) = (service.name.as_str(), service.pkg());
    let files = check.repo.files(&pkg);
    for (_, file) in OWN_FILES.iter().filter(|(owner, _)| *owner == svc) {
        check.require(files.contains(*file), || {
            format!("{svc}: missing pkg/{file}")
        });
    }
    if svc == "sentinel" {
        sentinel(check, service);
    }
    if svc == SVBONY {
        svbony_helper(check, service, shared);
    }
}

/// Doctor and its TLS renewal ride in sentinel's package: no doctor package
/// exists.
fn sentinel(check: &mut Check, service: &Service) {
    let (svc, pkg) = (&service.name, service.pkg());
    if let Some(unit) = check.read(&format!("{pkg}/rusty-photon-renew.service")) {
        for (line, why) in [
            ("Type=oneshot", "must be Type=oneshot"),
            (
                "ExecStart=/usr/bin/rusty-photon-doctor tls renew",
                "must ExecStart rusty-photon-doctor tls renew",
            ),
            ("User=rusty-photon", "must run as the service user"),
        ] {
            check.require(has_line(&unit, line), || {
                format!("{svc}: rusty-photon-renew.service {why}")
            });
        }
        check.require(!has_line_starting(&unit, "[Install]"), || {
            format!("{svc}: rusty-photon-renew.service must stay static (no [Install]; the timer arms it)")
        });
    }
    if let Some(timer) = check.read(&format!("{pkg}/rusty-photon-renew.timer")) {
        for (line, why) in [
            ("OnCalendar=daily", "must fire OnCalendar=daily"),
            (
                "Persistent=true",
                "must be Persistent (catch up after power-off)",
            ),
            ("WantedBy=timers.target", "must be WantedBy=timers.target"),
        ] {
            check.require(has_line(&timer, line), || {
                format!("{svc}: rusty-photon-renew.timer {why}")
            });
        }
    }
    check.require(
        service.deb_ships([
            "target/release/doctor",
            "usr/bin/rusty-photon-doctor",
            "755",
        ]),
        || {
            format!(
                "{svc}: deb assets must ship target/release/doctor as usr/bin/rusty-photon-doctor"
            )
        },
    );
    let renew_unit = service
        .deb_units()
        .iter()
        .any(|unit| string(Some(unit), "unit-name") == Some("rusty-photon-renew"));
    check.require(renew_unit, || {
        format!(
            "{svc}: deb systemd-units must carry a rusty-photon-renew entry (enables the timer)"
        )
    });
    for (dest, what) in [
        ("/usr/bin/rusty-photon-doctor", "the doctor binary"),
        (
            "/usr/lib/systemd/system/rusty-photon-renew.service",
            "rusty-photon-renew.service",
        ),
        (
            "/usr/lib/systemd/system/rusty-photon-renew.timer",
            "rusty-photon-renew.timer",
        ),
    ] {
        check.require(service.rpm_ships("dest", dest), || {
            format!("{svc}: rpm assets must ship {what}")
        });
    }
    let enables_timer = string(service.rpm(), "post_install_script")
        .is_some_and(|script| script.contains("systemctl enable rusty-photon-renew.timer"));
    check.require(enables_timer, || {
        format!("{svc}: rpm post_install must enable rusty-photon-renew.timer")
    });
}

/// The directory `-Wl,-rpath,<dir>"` names in the first line that has one.
fn rpath(build_packages: &str) -> Option<&str> {
    build_packages.lines().find_map(|line| {
        line.rmatch_indices("-Wl,-rpath,").find_map(|(at, flag)| {
            let rest = line.get(at.checked_add(flag.len())?..)?;
            rest.split_once('"').map(|(dir, _)| dir)
        })
    })
}

/// The vendor ID of the first `ATTRS{idVendor}=="<id>"` in a udev rule.
fn vendor_id(rule: &str) -> Option<&str> {
    rule.lines().find_map(|line| {
        let (_, rest) = line.split_once("ATTRS{idVendor}==\"")?;
        let (id, _) = rest.split_once('"')?;
        id.bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
            .then_some(id)
    })
}

/// `SVBony`'s SDK has no license grant, so its package ships a helper that
/// downloads it on the target (ADR-018).
fn svbony_helper(check: &mut Check, service: &Service, shared: &Shared) {
    let (svc, pkg) = (&service.name, service.pkg());
    if let Some(helper) = check.read(SVBONY_HELPER) {
        // Nothing else makes the loader look in the RUNPATH, which is off
        // ldconfig's path on purpose (ADR-013, ADR-018).
        let rpath = shared.build_packages.as_deref().and_then(rpath);
        let installs_there = has_line(&helper, "DEST=\"$DEST_DIR/libSVBCameraSDK.so\"")
            && rpath.is_some_and(|dir| has_line(&helper, &format!("DEST_DIR=\"$ROOT{dir}\"")));
        check.require(installs_there, || {
            format!(
                "{svc}: helper must install libSVBCameraSDK.so into build-packages.sh's RUNPATH ({})",
                rpath.unwrap_or_default()
            )
        });
        // The development rule must name the packaged rule's vendor, under
        // a name of its own: an /etc rule of the packaged one's name would
        // mask it once the package is installed.
        let rule = check
            .repo
            .read(&format!("{pkg}/90-rusty-photon-svbony.rules"))
            .unwrap_or_default();
        let vendor = vendor_id(&rule).unwrap_or_default();
        check.require(
            !vendor.is_empty() && helper.contains(&format!("ATTRS{{idVendor}}==\"{vendor}\"")),
            || format!("{svc}: helper's udev rule must match the packaged rule's VID ({vendor})"),
        );
        check.require(
            has_line(&helper, "DEV_RULE=90-rusty-photon-svbony-dev.rules"),
            || {
                format!(
                    "{svc}: helper's dev udev rule must not reuse 90-rusty-photon-svbony.rules \
                     (it would mask the packaged rule)"
                )
            },
        );
        check.require(!helper.contains("MODE=\"0666\""), || {
            format!("{svc}: helper must not install a world-writable udev rule (ADR-013 §3)")
        });
    }
    check.require(
        service.deb_ships([
            "pkg/rusty-photon-svbony-sdk-install",
            "usr/sbin/rusty-photon-svbony-sdk-install",
            "755",
        ]),
        || format!("{svc}: deb assets must ship rusty-photon-svbony-sdk-install"),
    );
    check.require(
        service.rpm_ships("dest", "/usr/sbin/rusty-photon-svbony-sdk-install"),
        || format!("{svc}: rpm assets must ship rusty-photon-svbony-sdk-install"),
    );
}

fn names(check: &mut Check, service: &Service) {
    let (svc, package) = (&service.name, service.package());
    check.require(string(service.deb(), "name") == Some(&package), || {
        format!("{svc}: [package.metadata.deb] name must be \"{package}\"")
    });
    let unit = service
        .deb_units()
        .iter()
        .any(|unit| string(Some(unit), "unit-name") == Some(&package));
    check.require(unit, || {
        format!("{svc}: [package.metadata.deb.systemd-units] unit-name must be \"{package}\"")
    });
    check.require(string(service.rpm(), "name") == Some(&package), || {
        format!("{svc}: [package.metadata.generate-rpm] name must be \"{package}\"")
    });
}

/// plugdev is Debian's group (base-passwd ships it): nothing may create it
/// on an rpm-family host, or depend on it there.
fn plugdev(check: &mut Check, services: &[Service], packaged: &[&Service]) {
    let creating: Vec<String> = services
        .iter()
        .filter(|service| service.text.contains("groupadd -r plugdev"))
        .map(|service| format!("services/{}/Cargo.toml", service.name))
        .collect();
    check.require(creating.is_empty(), || {
        format!(
            "rpm scriptlets must never create plugdev (Debian-only group): {}",
            creating.join(" ")
        )
    });
    for service in packaged {
        let dir = format!("{}/rpm", service.pkg());
        for unit in check.repo.files(&dir) {
            if !unit.ends_with(".service") {
                continue;
            }
            let path = format!("{dir}/{unit}");
            if let Some(text) = check.read(&path) {
                check.require(!text.contains("plugdev"), || {
                    format!("{path}: rpm units must not reference plugdev")
                });
            }
        }
    }
}

/// qhy-camera's `PINNED_SDK_VERSION`, as `YY.MM.DD`.
fn preflight_version(preflight: &str) -> Option<String> {
    let lines = preflight
        .lines()
        .skip_while(|line| !line.contains("PINNED_SDK_VERSION: PinnedSdkVersion = "));
    let (mut year, mut month, mut day) = (None, None, None);
    for line in lines {
        let mut words = line.split_whitespace();
        let field = match words.next() {
            Some("year:") => &mut year,
            Some("month:") => &mut month,
            Some("day:") => &mut day,
            _ => {
                if line.contains("};") {
                    break;
                }
                continue;
            }
        };
        *field = words
            .next()
            .and_then(|number| number.trim_end_matches(',').parse::<u8>().ok());
    }
    Some(format!("{:02}.{:02}.{:02}", year?, month?, day?))
}

/// The SDK pins copied between the packaging scripts, the shipped helpers,
/// the install actions and the crates, so the SDK CI links is the one the
/// packages ship and report.
fn sdk_pins(check: &mut Check, shared: &Shared) {
    let bp = shared.build_packages.as_deref().unwrap_or_default();
    let zwo = shared.zwo_module.as_deref().unwrap_or_default();
    let svbony = shared.svbony_module.as_deref().unwrap_or_default();
    let qhy_version = shell_var(bp, "QHY_SDK_VERSION");

    let helper = check.read(QHY_HELPER).unwrap_or_default();
    agree(
        check,
        "QHY SDK version",
        ("build-packages.sh", qhy_version),
        ("firmware-install", shell_var(&helper, "VERSION")),
    );
    for arch in ["X86_64", "AARCH64"] {
        agree(
            check,
            &format!("QHY SDK sha256 ({arch})"),
            (
                "build-packages.sh",
                shell_var(bp, &format!("QHY_SHA256_{arch}")),
            ),
            (
                "firmware-install",
                shell_var(&helper, &format!("SHA256_{arch}")),
            ),
        );
    }
    let msi = shared.build_msi.as_deref().unwrap_or_default();
    agree(
        check,
        "QHY SDK version",
        ("build-packages.sh", qhy_version),
        ("build-msi.ps1", ps_var(msi, "QhySdkVersion")),
    );
    if let (Some(build_rs), Some(version)) = (check.read(QHY_BUILD_RS), qhy_version) {
        check.require(build_rs.contains(&format!("sdk_win64_{version}")), || {
            format!("QHY SDK version pin mismatch: libqhyccd-sys build.rs has no sdk_win64_{version} search path")
        });
    }
    let preflight = check.read(QHY_PREFLIGHT).unwrap_or_default();
    agree(
        check,
        "QHY SDK version",
        ("build-packages.sh", qhy_version),
        (
            "preflight PINNED_SDK_VERSION",
            preflight_version(&preflight).as_deref(),
        ),
    );

    agree(
        check,
        "ZWO SDK ref",
        ("build-packages.sh", shell_var(bp, "ZWO_SDK_REF")),
        ("install-zwo-sdk's REF", js_const(zwo, "REF")),
    );
    // No ref exists for ZWO's Windows SDKs, only rolling "latest" links, so
    // the links are the whole pin.
    for (constant, variable) in [
        ("CAMERA_SDK_URL", "ZwoCameraSdkUrl"),
        ("EAF_SDK_URL", "ZwoEafSdkUrl"),
    ] {
        agree(
            check,
            &format!("ZWO Windows SDK URL ({constant})"),
            ("install-zwo-sdk", js_const(zwo, constant)),
            ("build-msi.ps1", ps_var(msi, variable)),
        );
    }
    if let Some(license) = check.read(ZWO_LICENSE) {
        for svc in ["zwo-camera", "zwo-focuser"] {
            if let Some(copy) = check.read(&format!("services/{svc}/pkg/ZWO-SDK-LICENSE.txt")) {
                check.require(copy == license, || {
                    format!("{svc}: pkg/ZWO-SDK-LICENSE.txt differs from {ZWO_LICENSE}")
                });
            }
        }
    }

    let svbony_helper = check.repo.read(SVBONY_HELPER).unwrap_or_default();
    agree(
        check,
        "SVBony SDK ref",
        (
            "rusty-photon-svbony-sdk-install",
            shell_var(&svbony_helper, "REF"),
        ),
        ("install-svbony-sdk's REF", js_const(svbony, "REF")),
    );
    agree(
        check,
        "SVBony SDK ref",
        ("build-packages.sh", shell_var(bp, "SVBONY_SDK_REF")),
        ("install-svbony-sdk's REF", js_const(svbony, "REF")),
    );
}

/// The case arm `build-tarballs.sh` leaves svbony-camera out with, the
/// service `cargo ci brew-formulas` renders no formula for.
const MACOS_EXCLUSION: &str = r#"*" svbony-camera "*)"#;

fn macos(check: &mut Check, shared: &Shared) {
    let bp = shared.build_packages.as_deref().unwrap_or_default();
    if let Some(tarballs) = check.read(BUILD_TARBALLS) {
        // Only the version is shared: the sha256s pin the macOS archives.
        agree(
            check,
            "QHY SDK version",
            ("build-packages.sh", shell_var(bp, "QHY_SDK_VERSION")),
            ("build-tarballs.sh", shell_var(&tarballs, "QHY_SDK_VERSION")),
        );
        agree(
            check,
            "ZWO SDK ref",
            ("build-tarballs.sh", shell_var(&tarballs, "ZWO_SDK_REF")),
            (
                "install-zwo-sdk's REF",
                js_const(shared.zwo_module.as_deref().unwrap_or_default(), "REF"),
            ),
        );
        check.require(tarballs.contains(MACOS_EXCLUSION), || {
            format!("{BUILD_TARBALLS}: svbony-camera exclusion guard missing (no confirmed mac_arm64 SVBony SDK blob)")
        });
    }
}

/// A `WiX` fragment's `Name` for its `<element ...>`: the first `Name=` line
/// after the element's opening line, as the fragments write one attribute
/// per line and `Id` on the opening one.
fn element_name<'a>(fragment: &'a str, element: &str) -> Option<&'a str> {
    let opening = format!("<{element} ");
    fragment
        .lines()
        .skip_while(|line| !line.contains(&opening))
        .skip(1)
        .find(|line| line.contains("Name=\""))
        .map(str::trim_start)
}

/// A `WiX` feature or component group's ID: the service's name in `PascalCase`.
fn feature_id(svc: &str) -> String {
    svc.split('-')
        .map(|word| {
            let mut chars = word.chars();
            chars.next().map_or_else(String::new, |first| {
                first.to_uppercase().chain(chars).collect()
            })
        })
        .collect()
}

/// The suite MSI: a feature per packaged service but svbony-camera.
fn installer(check: &mut Check, packaged: &[&Service], shared: &Shared) {
    let Some(package_wxs) = check.read(PACKAGE_WXS) else {
        return;
    };
    let linux_ports = shared.verify_packages.as_deref().unwrap_or_default();
    let windows: Vec<&str> = packaged
        .iter()
        .map(|service| service.name.as_str())
        .filter(|svc| *svc != SVBONY)
        .collect();
    for svc in &windows {
        let Some(fragment) = check.read(&format!("{FRAGMENTS}/{svc}.wxs")) else {
            continue;
        };
        fragment_contract(check, svc, &fragment, port_of(linux_ports, svc));
        let feature = feature_id(svc);
        check.require(
            package_wxs.contains(&format!("ComponentGroupRef Id=\"{feature}Components\"")),
            || format!("{svc}: Package.wxs does not reference {feature}Components (missing feature wiring)"),
        );
    }
    for file in check.repo.files(FRAGMENTS) {
        let Some(svc) = file.strip_suffix(".wxs") else {
            continue;
        };
        // The license both zwo features install.
        check.require(windows.contains(&svc) || svc == "zwo-sdk-license", || {
            format!("installer: orphan fragment {FRAGMENTS}/{file} (no matching packaged service)")
        });
    }

    // Doctor rides in Core with sentinel: no doctor package exists.
    if let Some(sentinel) = check.read(&format!("{FRAGMENTS}/sentinel.wxs")) {
        check.require(
            sentinel.contains(r#"Name="rusty-photon-doctor.exe" Source="!(bindpath.bin)\doctor.exe""#),
            || r"installer: sentinel fragment must install bindpath.bin\doctor.exe as rusty-photon-doctor.exe".to_owned(),
        );
    }
    for (text, problem) in [
        (
            r#"Id="RegisterRenewTask""#,
            "Package.wxs must register the rusty-photon-renew scheduled task",
        ),
        (
            r#"Id="UnregisterRenewTask""#,
            "Package.wxs must unregister the renewal task on uninstall",
        ),
        (
            "tls renew",
            "the renewal task must run rusty-photon-doctor tls renew",
        ),
    ] {
        check.require(package_wxs.contains(text), || {
            format!("installer: {problem}")
        });
    }
    let msi = shared.build_msi.as_deref().unwrap_or_default();
    let builds_doctor = msi
        .lines()
        .skip_while(|line| !line.contains("$allServices = @("))
        .take_while(|line| !line.starts_with(')'))
        .any(|line| line.contains("\"doctor\""));
    check.require(builds_doctor, || {
        "installer: build-msi.ps1 $allServices must include doctor".to_owned()
    });
}

/// What one service's fragment must say.
fn fragment_contract(check: &mut Check, svc: &str, fragment: &str, port: Option<&str>) {
    for element in ["ServiceInstall", "ServiceControl"] {
        let name = element_name(fragment, element);
        check.require(
            name == Some(&format!("Name=\"rusty-photon-{svc}\"")),
            || {
                format!(
                    "{svc}: {element} name must be rusty-photon-{svc} (got: {})",
                    name.unwrap_or("none")
                )
            },
        );
    }
    let exe = format!(r#"Name="rusty-photon-{svc}.exe" Source="!(bindpath.bin)\{svc}.exe""#);
    check.require(fragment.contains(&exe), || {
        format!(r"{svc}: fragment must install bindpath.bin\{svc}.exe as rusty-photon-{svc}.exe")
    });
    for (text, problem) in [
        (
            r#"Arguments="--service""#,
            "ServiceInstall must pass --service (SCM mode)",
        ),
        (
            r#"RestartServiceDelayInSeconds="5""#,
            "util:ServiceConfig must restart after 5 s (systemd RestartSec=5 parity)",
        ),
        (
            r#"FailureActionsWhen="failedToStopOrReturnedError""#,
            "native ServiceConfig must set the failure-actions flag \
             (ServiceSpecific(1) exits must count as failures)",
        ),
    ] {
        check.require(fragment.contains(text), || format!("{svc}: {problem}"));
    }
    // A service missing from the port table is named by `port_tables`.
    if let Some(port) = port {
        check.require(fragment.contains(&format!("Port=\"{port}\"")), || {
            format!("{svc}: firewall exception port must be {port}")
        });
    }
    let started = fragment.contains(r#"Start="install""#);
    if GATED.contains(&svc) {
        check.require(fragment.contains(r#"Start="demand""#), || {
            format!("{svc}: gated service must install with Start=\"demand\"")
        });
        check.require(!started, || {
            format!("{svc}: gated service must not be started by the installer")
        });
    } else {
        check.require(fragment.contains(r#"Start="auto""#), || {
            format!("{svc}: service must install with Start=\"auto\"")
        });
        check.require(started, || {
            format!("{svc}: ServiceControl must start the service on install")
        });
    }
}

/// A verify script's `port_of()` entry for `svc`: a `<svc>) echo <port> ;;`
/// line.
fn port_of<'a>(script: &'a str, svc: &str) -> Option<&'a str> {
    let arm = format!("{svc}) echo ");
    script.lines().find_map(|line| {
        let port = line
            .trim_start_matches(' ')
            .strip_prefix(&arm)?
            .strip_suffix(" ;;")?;
        (!port.is_empty() && port.bytes().all(|byte| byte.is_ascii_digit())).then_some(port)
    })
}

/// Each verify script keeps a port table. A service missing from one shows
/// only when that platform's nightly starts it.
fn port_tables(check: &mut Check, packaged: &[&Service], shared: &Shared) {
    let (Some(linux), Some(brew)) = (shared.verify_packages.as_deref(), check.read(VERIFY_BREW))
    else {
        return;
    };
    for service in packaged {
        let svc = &service.name;
        let Some(port) = port_of(linux, svc) else {
            check.problems.push(format!(
                "{svc}: no port mapping in {VERIFY_PACKAGES} port_of() — its verify leg would fail; extend it"
            ));
            continue;
        };
        if svc == SVBONY {
            continue;
        }
        match port_of(&brew, svc) {
            None => check.problems.push(format!(
                "{svc}: no port mapping in {VERIFY_BREW} port_of() — the macOS verify leg would fail; extend it"
            )),
            Some(brew_port) => check.require(brew_port == port, || {
                format!(
                    "{svc}: port mismatch — {VERIFY_PACKAGES} port_of() says {port}, \
                     {VERIFY_BREW} port_of() says {brew_port}"
                )
            }),
        }
    }
}

#[test]
fn the_packaging_files_agree() {
    let problems = problems(&Repo::new());
    assert!(problems.is_empty(), "{}", problems.join("\n"));
}

/// A change a test makes to the repository's files.
#[derive(Debug)]
enum Change {
    /// Replaces every `from` in the file, which must hold one, with `to`.
    Replace(&'static str, &'static str, &'static str),
    Remove(&'static str),
    Add(&'static str, &'static str),
}

/// The problems `changes` add to the repository's own.
fn added(before: &[String], changes: &[Change]) -> Vec<String> {
    let mut repo = Repo::new();
    for change in changes {
        match *change {
            Change::Replace(path, from, to) => {
                let text = repo
                    .read(path)
                    .unwrap_or_else(|error| panic!("{path}: {error}"));
                assert!(text.contains(from), "{path} holds no {from:?}");
                repo.changes
                    .insert(path.to_owned(), Some(text.replace(from, to)));
            }
            Change::Remove(path) => {
                assert!(repo.read(path).is_ok(), "{path} is not there");
                repo.changes.insert(path.to_owned(), None);
            }
            Change::Add(path, text) => {
                assert!(repo.read(path).is_err(), "{path} is already there");
                repo.changes.insert(path.to_owned(), Some(text.to_owned()));
            }
        }
    }
    problems(&repo)
        .into_iter()
        .filter(|problem| !before.contains(problem))
        .collect()
}

/// Each mistake, made in the repository's own files, and what the problems
/// it adds say: each added problem holds one of these, and each of these is
/// in one.
type Mistake = (&'static [Change], &'static [&'static str]);

/// Runs each mistake and names the ones whose problems are not as listed.
fn assert_named(mistakes: &[Mistake]) {
    let before = problems(&Repo::new());
    let failures: Vec<String> = mistakes
        .iter()
        .filter_map(|(changes, expected)| {
            let added = added(&before, changes);
            let named = added
                .iter()
                .all(|problem| expected.iter().any(|text| problem.contains(text)))
                && expected
                    .iter()
                    .all(|text| added.iter().any(|problem| problem.contains(text)));
            (!named).then(|| format!("{changes:?}\n  expected: {expected:?}\n  added: {added:?}"))
        })
        .collect();
    assert!(failures.is_empty(), "{}", failures.join("\n\n"));
}

const FILEMONITOR_UNIT: &str = "services/filemonitor/pkg/rusty-photon-filemonitor.service";
const PPBA_DEB_UNIT: &str = "services/ppba-driver/pkg/deb/rusty-photon-ppba-driver.service";
const PPBA_RPM_UNIT: &str = "services/ppba-driver/pkg/rpm/rusty-photon-ppba-driver.service";
const PPBA_TOML: &str = "services/ppba-driver/Cargo.toml";
const ZWO_RULE: &str = "services/zwo-camera/pkg/90-rusty-photon-zwo.rules";
const SVBONY_POSTINST: &str = "services/svbony-camera/pkg/postinst";
const SVBONY_TOML: &str = "services/svbony-camera/Cargo.toml";
const RENEW_UNIT: &str = "services/sentinel/pkg/rusty-photon-renew.service";
const RENEW_TIMER: &str = "services/sentinel/pkg/rusty-photon-renew.timer";
const SENTINEL_TOML: &str = "services/sentinel/Cargo.toml";
const PPBA_FRAGMENT: &str = "installer/fragments/ppba-driver.wxs";
const PLATE_SOLVER_FRAGMENT: &str = "installer/fragments/plate-solver.wxs";

#[test]
fn names_each_mistake_in_a_unit() {
    use Change::{Remove, Replace};
    assert_named(&[
        (
            &[Replace(
                FILEMONITOR_UNIT,
                "ExecStart=/usr/bin/rusty-photon-filemonitor\n",
                "ExecStart=/usr/bin/rusty-photon-filemonitor --config x\n",
            )],
            &["filemonitor: ExecStart must be exactly /usr/bin/rusty-photon-filemonitor"],
        ),
        (
            &[Replace(
                FILEMONITOR_UNIT,
                "ExecReload=/bin/kill -HUP $MAINPID\n",
                "",
            )],
            &["filemonitor: reload-capable service must have ExecReload"],
        ),
        (
            &[Replace(
                "services/plate-solver/pkg/rusty-photon-plate-solver.service",
                "ConditionPathExists=",
                "#ConditionPathExists=",
            )],
            &["plate-solver: no-default-config service must gate"],
        ),
        (&[Remove(PPBA_RPM_UNIT)], &[PPBA_RPM_UNIT]),
        (
            &[Replace(
                PPBA_DEB_UNIT,
                "SupplementaryGroups=dialout plugdev",
                "SupplementaryGroups=dialout",
            )],
            &["ppba-driver: deb unit must carry SupplementaryGroups=dialout plugdev"],
        ),
        (
            &[Replace(
                PPBA_RPM_UNIT,
                "SupplementaryGroups=dialout",
                "SupplementaryGroups=dialout plugdev",
            )],
            &[
                "ppba-driver: rpm unit must carry exactly SupplementaryGroups=dialout",
                "rusty-photon-ppba-driver.service: rpm units must not reference plugdev",
            ],
        ),
        (
            &[Replace(PPBA_RPM_UNIT, "RestartSec=5", "RestartSec=10")],
            &["ppba-driver: deb and rpm units must differ only in the SupplementaryGroups line"],
        ),
        (
            &[Replace(
                PPBA_TOML,
                "unit-scripts = \"pkg/deb/\"",
                "unit-scripts = \"pkg/\"",
            )],
            &["ppba-driver: deb systemd-units must take the unit from pkg/deb/"],
        ),
        (
            &[Replace(
                PPBA_TOML,
                "source = \"pkg/rpm/rusty-photon-ppba-driver.service\"",
                "source = \"pkg/rusty-photon-ppba-driver.service\"",
            )],
            &["ppba-driver: rpm assets must ship the pkg/rpm/ unit"],
        ),
        (
            &[Replace(
                ZWO_RULE,
                "GROUP=\"rusty-photon\"",
                "GROUP=\"plugdev\"",
            )],
            &[
                "zwo-camera: 90-rusty-photon-zwo.rules must assign GROUP=\"rusty-photon\"",
                "zwo-camera: 90-rusty-photon-zwo.rules must not assign plugdev",
            ],
        ),
        (
            &[Replace(
                "services/zwo-camera/pkg/rusty-photon-zwo-camera.service",
                "[Service]\n",
                "[Service]\nSupplementaryGroups=video\n",
            )],
            &["zwo-camera: camera unit needs no SupplementaryGroups"],
        ),
    ]);
}

#[test]
fn names_each_mistake_in_a_maintainer_script() {
    use Change::Replace;
    assert_named(&[
        (
            &[Replace("services/filemonitor/pkg/postrm", "#DEBHELPER#", "true\n#DEBHELPER#")],
            &["filemonitor: pkg/postrm differs from packaging/postrm.common"],
        ),
        (
            &[Replace("services/filemonitor/pkg/postinst", "#DEBHELPER#", "true\n#DEBHELPER#")],
            &["filemonitor: pkg/postinst differs from packaging/postinst.common"],
        ),
        (
            &[Replace("services/zwo-camera/pkg/postinst", "udevadm trigger", "udevadm settle")],
            &["zwo-camera: pkg/postinst must be postinst.common + udev stanza before #DEBHELPER#"],
        ),
        (
            &[Replace(SVBONY_POSTINST, "adduser --system", "adduser --system --no-create-home")],
            &["svbony-camera: pkg/postinst's user/dir/symlink preamble must match"],
        ),
        (
            &[Replace(SVBONY_POSTINST, "udevadm control --reload-rules || true\n", "")],
            &["svbony-camera: pkg/postinst must reload udev rules"],
        ),
        (
            &[Replace(SVBONY_POSTINST, "udevadm trigger || true\n", "")],
            &["svbony-camera: pkg/postinst must trigger udev"],
        ),
        (
            &[Replace(SVBONY_POSTINST, "' once as root", "' as root")],
            &["svbony-camera: pkg/postinst must point the operator at rusty-photon-svbony-sdk-install"],
        ),
        (
            &[Replace(SVBONY_POSTINST, "\n#DEBHELPER#", "\n# #DEBHELPER#")],
            &["svbony-camera: pkg/postinst must end with #DEBHELPER#"],
        ),
    ]);
}

#[test]
fn names_each_mistake_in_a_packages_own_files() {
    use Change::Remove;
    assert_named(&[
        (
            &[Remove(
                "services/sentinel/pkg/50-rusty-photon-sentinel.rules",
            )],
            &["sentinel: missing pkg/50-rusty-photon-sentinel.rules"],
        ),
        (
            &[Remove("services/qhy-camera/pkg/90-rusty-photon-qhy.rules")],
            &["qhy-camera: missing pkg/90-rusty-photon-qhy.rules"],
        ),
        (
            &[Remove(ZWO_RULE)],
            &["zwo-camera: missing pkg/90-rusty-photon-zwo.rules"],
        ),
        (
            &[Remove(
                "services/zwo-focuser/pkg/90-rusty-photon-zwo-focuser.rules",
            )],
            &["zwo-focuser: missing pkg/90-rusty-photon-zwo-focuser.rules"],
        ),
        (
            &[Remove(
                "services/svbony-camera/pkg/90-rusty-photon-svbony.rules",
            )],
            &[
                "svbony-camera: missing pkg/90-rusty-photon-svbony.rules",
                "svbony-camera: helper's udev rule must match the packaged rule's VID",
            ],
        ),
    ]);
}

#[test]
fn names_each_mistake_in_sentinels_doctor_and_renewal() {
    use Change::{Remove, Replace};
    assert_named(&[
        (&[Remove(RENEW_UNIT)], &[RENEW_UNIT]),
        (&[Remove(RENEW_TIMER)], &[RENEW_TIMER]),
        (
            &[Replace(RENEW_UNIT, "Type=oneshot", "Type=simple")],
            &["sentinel: rusty-photon-renew.service must be Type=oneshot"],
        ),
        (
            &[Replace(RENEW_UNIT, "tls renew", "tls check")],
            &["sentinel: rusty-photon-renew.service must ExecStart rusty-photon-doctor tls renew"],
        ),
        (
            &[Replace(RENEW_UNIT, "User=rusty-photon\n", "")],
            &["sentinel: rusty-photon-renew.service must run as the service user"],
        ),
        (
            &[Replace(
                RENEW_UNIT,
                "UMask=0027\n",
                "UMask=0027\n\n[Install]\nWantedBy=multi-user.target\n",
            )],
            &["sentinel: rusty-photon-renew.service must stay static"],
        ),
        (
            &[Replace(
                RENEW_TIMER,
                "OnCalendar=daily",
                "OnCalendar=weekly",
            )],
            &["sentinel: rusty-photon-renew.timer must fire OnCalendar=daily"],
        ),
        (
            &[Replace(RENEW_TIMER, "Persistent=true", "Persistent=false")],
            &["sentinel: rusty-photon-renew.timer must be Persistent"],
        ),
        (
            &[Replace(
                RENEW_TIMER,
                "WantedBy=timers.target",
                "WantedBy=multi-user.target",
            )],
            &["sentinel: rusty-photon-renew.timer must be WantedBy=timers.target"],
        ),
        (
            &[Replace(
                SENTINEL_TOML,
                "[\"target/release/doctor\", \"usr/bin/rusty-photon-doctor\", \"755\"],",
                "",
            )],
            &["sentinel: deb assets must ship target/release/doctor"],
        ),
        (
            &[Replace(
                SENTINEL_TOML,
                "unit-name = \"rusty-photon-renew\"",
                "unit-name = \"rusty-photon-renewal\"",
            )],
            &["sentinel: deb systemd-units must carry a rusty-photon-renew entry"],
        ),
        (
            &[Replace(
                SENTINEL_TOML,
                "dest = \"/usr/bin/rusty-photon-doctor\"",
                "dest = \"/usr/bin/doctor\"",
            )],
            &["sentinel: rpm assets must ship the doctor binary"],
        ),
        (
            &[Replace(
                SENTINEL_TOML,
                "dest = \"/usr/lib/systemd/system/rusty-photon-renew.service\"",
                "dest = \"/usr/lib/systemd/system/renew.service\"",
            )],
            &["sentinel: rpm assets must ship rusty-photon-renew.service"],
        ),
        (
            &[Replace(
                SENTINEL_TOML,
                "dest = \"/usr/lib/systemd/system/rusty-photon-renew.timer\"",
                "dest = \"/usr/lib/systemd/system/renew.timer\"",
            )],
            &["sentinel: rpm assets must ship rusty-photon-renew.timer"],
        ),
        (
            &[Replace(
                SENTINEL_TOML,
                "    systemctl enable rusty-photon-renew.timer\n",
                "",
            )],
            &["sentinel: rpm post_install must enable rusty-photon-renew.timer"],
        ),
    ]);
}

#[test]
fn names_each_mistake_in_svbony_cameras_sdk_helper() {
    use Change::{Remove, Replace};
    assert_named(&[
        (
            &[Remove(SVBONY_HELPER)],
            &[
                "cannot read services/svbony-camera/pkg/rusty-photon-svbony-sdk-install",
                "SVBony SDK ref pin mismatch: rusty-photon-svbony-sdk-install=''",
            ],
        ),
        (
            &[Replace(SVBONY_HELPER, "DEST_DIR=\"$ROOT/usr/lib/rusty-photon\"", "DEST_DIR=\"$ROOT/usr/lib\"")],
            &["svbony-camera: helper must install libSVBCameraSDK.so into build-packages.sh's RUNPATH (/usr/lib/rusty-photon)"],
        ),
        (
            &[Replace(BUILD_PACKAGES, "-Wl,-rpath,/usr/lib/rusty-photon\"", "-Wl,-rpath,/opt/rusty-photon\"")],
            &["svbony-camera: helper must install libSVBCameraSDK.so into build-packages.sh's RUNPATH (/opt/rusty-photon)"],
        ),
        (
            &[Replace(SVBONY_HELPER, "ATTRS{idVendor}==\"", "ATTRS{idVendor}==\"0")],
            &["svbony-camera: helper's udev rule must match the packaged rule's VID"],
        ),
        (
            &[Replace(SVBONY_HELPER, "DEV_RULE=90-rusty-photon-svbony-dev.rules", "DEV_RULE=90-rusty-photon-svbony.rules")],
            &["svbony-camera: helper's dev udev rule must not reuse 90-rusty-photon-svbony.rules"],
        ),
        (
            &[Replace(SVBONY_HELPER, "DEV_RULE=90-rusty-photon-svbony-dev.rules\n", "DEV_RULE=90-rusty-photon-svbony-dev.rules\n# MODE=\"0666\"\n")],
            &["svbony-camera: helper must not install a world-writable udev rule"],
        ),
        (
            &[Replace(
                SVBONY_TOML,
                "\"usr/sbin/rusty-photon-svbony-sdk-install\", \"755\"",
                "\"usr/bin/rusty-photon-svbony-sdk-install\", \"755\"",
            )],
            &["svbony-camera: deb assets must ship rusty-photon-svbony-sdk-install"],
        ),
        (
            &[Replace(
                SVBONY_TOML,
                "dest = \"/usr/sbin/rusty-photon-svbony-sdk-install\"",
                "dest = \"/usr/bin/rusty-photon-svbony-sdk-install\"",
            )],
            &["svbony-camera: rpm assets must ship rusty-photon-svbony-sdk-install"],
        ),
    ]);
}

#[test]
fn names_each_mistake_in_a_name_or_the_service_list() {
    use Change::{Add, Replace};
    assert_named(&[
        (
            &[Replace(PPBA_TOML, "\nname = \"rusty-photon-ppba-driver\"", "\nname = \"ppba-driver\"")],
            &[
                "ppba-driver: [package.metadata.deb] name must be \"rusty-photon-ppba-driver\"",
                "ppba-driver: [package.metadata.generate-rpm] name must be \"rusty-photon-ppba-driver\"",
            ],
        ),
        (
            &[Replace(PPBA_TOML, "unit-name = \"rusty-photon-ppba-driver\"", "unit-name = \"ppba-driver\"")],
            &["ppba-driver: [package.metadata.deb.systemd-units] unit-name must be \"rusty-photon-ppba-driver\""],
        ),
        (
            &[Replace("Cargo.toml", "  \"services/zwo-focuser\",\n", "  \"services/zwo-focuser\",\n  \"services/ghost\",\n")],
            &["cannot read services/ghost/Cargo.toml"],
        ),
        // A packaged service whose pkg/ the test cannot see is named, with
        // everything its package lacks.
        (
            &[
                Replace("Cargo.toml", "  \"services/zwo-focuser\",\n", "  \"services/zwo-focuser\",\n  \"services/new-thing\",\n"),
                Add(
                    "services/new-thing/Cargo.toml",
                    "[package]\nname = \"new-thing\"\n\n[package.metadata.deb]\nname = \"rusty-photon-new-thing\"\n",
                ),
            ],
            &["[package.metadata.deb] but no pkg/ directory", "new-thing"],
        ),
        (
            &[Replace(
                "services/filemonitor/Cargo.toml",
                "useradd -r -d",
                "groupadd -r plugdev; useradd -r -d",
            )],
            &["rpm scriptlets must never create plugdev (Debian-only group): services/filemonitor/Cargo.toml"],
        ),
    ]);
}

#[test]
fn names_each_mistake_in_an_sdk_pin() {
    use Change::Replace;
    assert_named(&[
        (
            &[Replace(QHY_HELPER, "VERSION=\"", "VERSION=\"9")],
            &["QHY SDK version pin mismatch: build-packages.sh='"],
        ),
        (
            &[Replace(QHY_HELPER, "SHA256_X86_64=\"", "SHA256_X86_64=\"0")],
            &["QHY SDK sha256 (X86_64) pin mismatch"],
        ),
        (
            &[Replace(QHY_HELPER, "SHA256_AARCH64=\"", "SHA256_AARCH64=\"0")],
            &["QHY SDK sha256 (AARCH64) pin mismatch"],
        ),
        (
            &[Replace(BUILD_MSI, "$QhySdkVersion = \"", "$QhySdkVersion = \"9")],
            &["' vs build-msi.ps1='9"],
        ),
        (
            &[Replace(QHY_BUILD_RS, "\"sdk_win64_", "\"sdk_win64_x")],
            &["QHY SDK version pin mismatch: libqhyccd-sys build.rs has no sdk_win64_"],
        ),
        (
            &[Replace(QHY_PREFLIGHT, "    day: ", "    day: 1")],
            &["' vs preflight PINNED_SDK_VERSION='"],
        ),
        (
            &[Replace(BUILD_TARBALLS, "QHY_SDK_VERSION=\"", "QHY_SDK_VERSION=\"9")],
            &["' vs build-tarballs.sh='9"],
        ),
        (
            &[Replace(BUILD_PACKAGES, "ZWO_SDK_REF=\"", "ZWO_SDK_REF=\"x")],
            &["ZWO SDK ref pin mismatch: build-packages.sh='x"],
        ),
        (
            &[Replace(BUILD_TARBALLS, "ZWO_SDK_REF=\"", "ZWO_SDK_REF=\"x")],
            &["ZWO SDK ref pin mismatch: build-tarballs.sh='x"],
        ),
        (
            &[Replace(BUILD_MSI, "app=DeveloperCameraSdk&", "app=DeveloperCameraSdkBeta&")],
            &["ZWO Windows SDK URL (CAMERA_SDK_URL) pin mismatch"],
        ),
        (
            &[Replace(BUILD_MSI, "app=DeveloperEafSdk&", "app=DeveloperEafSdkBeta&")],
            &["ZWO Windows SDK URL (EAF_SDK_URL) pin mismatch"],
        ),
        (
            &[Replace("services/zwo-focuser/pkg/ZWO-SDK-LICENSE.txt", "ZWO Company", "ZWO Co.")],
            &["zwo-focuser: pkg/ZWO-SDK-LICENSE.txt differs from crates/zwo-rs/libzwo-sys/sdk/include/license.txt"],
        ),
        (
            &[Replace(SVBONY_HELPER, "\nREF=\"", "\nREF=\"x")],
            &["SVBony SDK ref pin mismatch: rusty-photon-svbony-sdk-install='x"],
        ),
        (
            &[Replace(BUILD_PACKAGES, "SVBONY_SDK_REF=\"", "SVBONY_SDK_REF=\"x")],
            &["SVBony SDK ref pin mismatch: build-packages.sh='x"],
        ),
    ]);
}

#[test]
fn names_each_mistake_for_macos() {
    use Change::Replace;
    assert_named(&[(
        &[Replace(BUILD_TARBALLS, MACOS_EXCLUSION, r#"*" svbony "*)"#)],
        &["scripts/build-tarballs.sh: svbony-camera exclusion guard missing"],
    )]);
}

#[test]
fn leaves_out_the_service_the_formula_renderer_does() {
    assert_eq!(
        MACOS_EXCLUSION,
        format!(r#"*" {} "*)"#, crate::brew_formulas::NO_MACOS_TARBALL)
    );
}

#[test]
fn names_each_mistake_in_the_installer() {
    use Change::{Add, Remove, Replace};
    const NAME: &str = "                        Name=\"rusty-photon-ppba-driver\"";
    assert_named(&[
        (
            &[Replace(
                PPBA_FRAGMENT,
                "Id=\"PpbaDriverService\"\n                        Name=\"rusty-photon-ppba-driver\"",
                "Id=\"PpbaDriverService\"\n                        Name=\"ppba-driver\"",
            )],
            &["ppba-driver: ServiceInstall name must be rusty-photon-ppba-driver (got: Name=\"ppba-driver\")"],
        ),
        (
            &[Replace(
                PPBA_FRAGMENT,
                "Id=\"PpbaDriverServiceControl\"\n                        Name=\"rusty-photon-ppba-driver\"",
                "Id=\"PpbaDriverServiceControl\"\n                        Name=\"ppba-driver\"",
            )],
            &["ppba-driver: ServiceControl name must be rusty-photon-ppba-driver (got: Name=\"ppba-driver\")"],
        ),
        (
            &[Replace(PPBA_FRAGMENT, NAME, "                        Id2=\"x\"")],
            &[
                "ppba-driver: ServiceInstall name must be rusty-photon-ppba-driver (got: DisplayName",
                "ppba-driver: ServiceControl name must be rusty-photon-ppba-driver (got:",
            ],
        ),
        (
            &[Replace(PPBA_FRAGMENT, r"!(bindpath.bin)\ppba-driver.exe", r"!(bindpath.bin)\ppba.exe")],
            &[r"ppba-driver: fragment must install bindpath.bin\ppba-driver.exe as rusty-photon-ppba-driver.exe"],
        ),
        (
            &[Replace(PPBA_FRAGMENT, "Arguments=\"--service\"", "Arguments=\"--console\"")],
            &["ppba-driver: ServiceInstall must pass --service"],
        ),
        (
            &[Replace(PPBA_FRAGMENT, "RestartServiceDelayInSeconds=\"5\"", "RestartServiceDelayInSeconds=\"10\"")],
            &["ppba-driver: util:ServiceConfig must restart after 5 s"],
        ),
        (
            &[Replace(
                PPBA_FRAGMENT,
                "FailureActionsWhen=\"failedToStopOrReturnedError\"",
                "FailureActionsWhen=\"failedToStop\"",
            )],
            &["ppba-driver: native ServiceConfig must set the failure-actions flag"],
        ),
        (
            &[Replace(PPBA_FRAGMENT, "Port=\"", "Port=\"9")],
            &["ppba-driver: firewall exception port must be"],
        ),
        (
            &[Replace(PLATE_SOLVER_FRAGMENT, "Start=\"demand\"", "Start=\"install\"")],
            &[
                "plate-solver: gated service must install with Start=\"demand\"",
                "plate-solver: gated service must not be started by the installer",
            ],
        ),
        (
            &[Replace(PPBA_FRAGMENT, "Start=\"auto\"", "Start=\"demand\"")],
            &["ppba-driver: service must install with Start=\"auto\""],
        ),
        (
            &[Replace(PPBA_FRAGMENT, "Start=\"install\"", "Start=\"never\"")],
            &["ppba-driver: ServiceControl must start the service on install"],
        ),
        (
            &[Replace(PACKAGE_WXS, "<ComponentGroupRef Id=\"PpbaDriverComponents\" />", "")],
            &["ppba-driver: Package.wxs does not reference PpbaDriverComponents"],
        ),
        (&[Remove(PPBA_FRAGMENT)], &["cannot read installer/fragments/ppba-driver.wxs"]),
        (
            &[Add("installer/fragments/ghost.wxs", "<Wix />\n")],
            &["installer: orphan fragment installer/fragments/ghost.wxs"],
        ),
        (
            &[Replace("installer/fragments/sentinel.wxs", r"!(bindpath.bin)\doctor.exe", r"!(bindpath.bin)\rp-doctor.exe")],
            &[r"installer: sentinel fragment must install bindpath.bin\doctor.exe"],
        ),
        (
            &[Replace(PACKAGE_WXS, "Id=\"RegisterRenewTask\"", "Id=\"RegisterRenewalTask\"")],
            &["installer: Package.wxs must register the rusty-photon-renew scheduled task"],
        ),
        (
            &[Replace(PACKAGE_WXS, "Id=\"UnregisterRenewTask\"", "Id=\"UnregisterRenewalTask\"")],
            &["installer: Package.wxs must unregister the renewal task on uninstall"],
        ),
        (
            &[Replace(PACKAGE_WXS, "tls renew", "tls check")],
            &["installer: the renewal task must run rusty-photon-doctor tls renew"],
        ),
        (
            &[Replace(BUILD_MSI, ", \"doctor\"", "")],
            &["installer: build-msi.ps1 $allServices must include doctor"],
        ),
    ]);
}

#[test]
fn names_each_mistake_in_a_port_table() {
    use Change::Replace;
    assert_named(&[
        (
            &[Replace(
                VERIFY_PACKAGES,
                "ppba-driver) echo ",
                "ppba-driver-gone) echo ",
            )],
            &["ppba-driver: no port mapping in scripts/verify-packages.sh port_of()"],
        ),
        (
            &[Replace(
                VERIFY_BREW,
                "ppba-driver) echo ",
                "ppba-driver-gone) echo ",
            )],
            &["ppba-driver: no port mapping in scripts/verify-brew.sh port_of()"],
        ),
        (
            &[Replace(
                VERIFY_BREW,
                "ppba-driver) echo ",
                "ppba-driver) echo 9",
            )],
            &["ppba-driver: port mismatch — scripts/verify-packages.sh port_of() says"],
        ),
        // The fragment's firewall port is the Linux table's.
        (
            &[Replace(
                VERIFY_PACKAGES,
                "ppba-driver) echo ",
                "ppba-driver) echo 9",
            )],
            &[
                "ppba-driver: firewall exception port must be 9",
                "ppba-driver: port mismatch — scripts/verify-packages.sh port_of() says 9",
            ],
        ),
    ]);
}

#[test]
fn reads_a_preflight_version_as_yy_mm_dd() {
    let preflight = "pub const PINNED_SDK_VERSION: PinnedSdkVersion = PinnedSdkVersion {\n    year: 26,\n    month: 6,\n    day: 4,\n};\n";
    assert_eq!(preflight_version(preflight).as_deref(), Some("26.06.04"));
    assert_eq!(
        preflight_version(&preflight.replace("    day: 4,\n", "")),
        None
    );
}

#[test]
fn puts_the_stanza_before_debhelper() {
    assert_eq!(
        with_stanza("a\n#DEBHELPER#\n", "b\nc"),
        "a\nb\nc\n#DEBHELPER#\n"
    );
}

#[test]
fn names_a_feature_in_pascal_case() {
    assert_eq!(feature_id("star-adventurer-gti"), "StarAdventurerGti");
}
