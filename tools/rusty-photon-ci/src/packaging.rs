//! What the packaging tools share.
//!
//! The packaged services and what kind of service each one is, the version
//! a build stamps on its artifacts, the native SDKs' pins and their
//! download, the cargo runs that build the services, and the checksum
//! manifest.
//!
//! A packaged service is one with a `services/<svc>/pkg` directory, the
//! packaging's authority since ADR-016 (decision 11). The ZWO and `SVBony`
//! SDKs are pinned once, by the commit their install action fetches
//! (`REF` in `.github/actions/install-zwo-sdk/zwo-sdk.mjs` and
//! `install-svbony-sdk/svbony-sdk.mjs`), which the builders read, so the
//! SDK CI links is the one the packages ship. The QHY SDK has no install
//! action; its pins are [`qhy`]'s.

use std::fmt::Write as _;
use std::fs;
use std::path::{Path, PathBuf};

use sha2::{Digest, Sha256};

use crate::process::{self, Cmd, Runner};

/// Serial drivers. They exit on an absent device, so a service manager
/// restarts them, and a verifier checks they tried the device rather than
/// that they serve.
pub const SERIAL: &[&str] = &[
    "ppba-driver",
    "upbv2-driver",
    "qhy-focuser",
    "pa-falcon-rotator",
    "pa-scops-oag",
    "dsd-fp2",
    "star-adventurer-gti",
];

/// Services with no config they can default.
///
/// Nothing starts them until an operator writes one: systemd waits for it, the MSI installs them
/// demand-start, and on macOS nothing runs until `brew services start`.
pub const GATED: &[&str] = &[
    "sky-survey-camera",
    "plate-solver",
    "calibrator-flats",
    "focus-model",
    "session-runner",
    "polar-align",
];

/// Services that run on built-in defaults and write no config file.
pub const NO_CONFIG_FILE: &[&str] = &["qhy-camera", "zwo-camera", "zwo-focuser", "phd2-guider"];

/// Services that answer `/health` rather than the Alpaca management API.
pub const HEALTH_PROBED: &[&str] = &[
    "sentinel",
    "rp",
    "ui-htmx",
    "phd2-guider",
    "session-runner",
    "polar-align",
    "focus-model",
];

/// Each packaged service's port, which a verifier probes and a `WiX` fragment
/// opens in the firewall.
pub const PORTS: &[(&str, u16)] = &[
    ("filemonitor", 11111),
    ("ppba-driver", 11112),
    ("qhy-focuser", 11113),
    ("sentinel", 11114),
    ("rp", 11115),
    ("sky-survey-camera", 11116),
    ("star-adventurer-gti", 11117),
    ("pa-falcon-rotator", 11118),
    ("dsd-fp2", 11119),
    ("ui-htmx", 11120),
    ("qhy-camera", 11121),
    ("zwo-camera", 11122),
    ("pa-scops-oag", 11123),
    ("zwo-focuser", 11124),
    ("svbony-camera", 11125),
    ("planetarium-bridge", 11126),
    ("upbv2-driver", 11127),
    ("phd2-guider", 11130),
    ("plate-solver", 11131),
    ("calibrator-flats", 11170),
    ("session-runner", 11171),
    ("polar-align", 11172),
    ("focus-model", 11173),
];

/// `service`'s port, from [`PORTS`].
#[must_use]
pub fn port(service: &str) -> Option<u16> {
    PORTS
        .iter()
        .find(|(name, _)| *name == service)
        .map(|&(_, port)| port)
}

/// The service with no macOS SDK.
///
/// indi-3rdparty ships `SVBony`'s Intel one only, and the Mac target is
/// arm64, so it gets no tarball and no formula: a
/// binary built without the SDK could never see a camera
/// (docs/services/svbony-camera.md, Packaging).
pub const NO_MACOS_SDK: &str = "svbony-camera";

/// The service the suite MSI has no feature for.
///
/// `SVBony`'s SDK for Windows downloads only through a CAPTCHA, and the
/// project's private mirror holds the one copy CI links (ADR-018 §7), which
/// nothing may ship (docs/services/svbony-camera.md, Windows).
pub const NO_WINDOWS_SDK: &str = "svbony-camera";

/// The QHY SDK's pins, in a file of their own: the packaging jobs' SDK
/// caches are keyed on it.
pub mod qhy;

/// The install modules whose `REF` pins the ZWO and `SVBony` SDKs.
pub const ZWO_MODULE: &str = ".github/actions/install-zwo-sdk/zwo-sdk.mjs";
pub const SVBONY_MODULE: &str = ".github/actions/install-svbony-sdk/svbony-sdk.mjs";

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error(transparent)]
    Process(#[from] process::Error),
    #[error("{path}: {source}")]
    Io {
        path: String,
        source: std::io::Error,
    },
    #[error("unknown or unpackaged service: {service} (packaged: {packaged})")]
    UnknownService { service: String, packaged: String },
    #[error(
        "svbony-camera has no macOS tarball (no confirmed mac_arm64 SVBony SDK blob — \
         see docs/services/svbony-camera.md Packaging)"
    )]
    NoMacosSdk,
    #[error("could not read the workspace version from Cargo.toml")]
    NoWorkspaceVersion,
    #[error("--version must be {workspace} or {workspace}+nightly.<stamp> (got: {asked})")]
    BadVersion { workspace: String, asked: String },
    #[error("{module} has no `export const {name} = '…';` line")]
    NoPin { module: String, name: &'static str },
    #[error("sha256 mismatch for {path} (expected {expected}, got {actual})")]
    Sha256Mismatch {
        path: String,
        expected: String,
        actual: String,
    },
}

/// The [`Error::Io`] for `path`.
pub fn io(path: &Path) -> impl FnOnce(std::io::Error) -> Error + '_ {
    move |source| Error::Io {
        path: path.display().to_string(),
        source,
    }
}

/// The repository's root.
///
/// # Errors
///
/// When git cannot say.
pub fn repo_root(runner: &mut impl Runner) -> Result<PathBuf, Error> {
    let root = runner.read(&Cmd::new("git", &["rev-parse", "--show-toplevel"]))?;
    Ok(PathBuf::from(root.trim_end()))
}

/// The packaged services under `root`, sorted.
///
/// # Errors
///
/// When `services/` cannot be listed.
pub fn packaged(root: &Path) -> Result<Vec<String>, Error> {
    let services = root.join("services");
    let mut names: Vec<String> = fs::read_dir(&services)
        .map_err(io(&services))?
        .filter_map(Result::ok)
        .filter(|entry| entry.path().join("pkg").is_dir())
        .filter_map(|entry| entry.file_name().into_string().ok())
        .collect();
    names.sort();
    Ok(names)
}

/// The services a macOS tool works on: the ones asked for, in that order, or
/// every packaged one but [`NO_MACOS_SDK`].
///
/// # Errors
///
/// When one asked for is not packaged, or is [`NO_MACOS_SDK`].
pub fn macos_services(packaged: &[String], asked: Option<&[String]>) -> Result<Vec<String>, Error> {
    let Some(asked) = asked else {
        return Ok(packaged
            .iter()
            .filter(|service| *service != NO_MACOS_SDK)
            .cloned()
            .collect());
    };
    for service in asked {
        if !packaged.contains(service) {
            return Err(Error::UnknownService {
                service: service.clone(),
                packaged: packaged.join(" "),
            });
        }
        if service == NO_MACOS_SDK {
            return Err(Error::NoMacosSdk);
        }
    }
    Ok(asked.to_vec())
}

/// The workspace's version, `[workspace.package]`'s in the root `Cargo.toml`.
///
/// # Errors
///
/// When the file cannot be read or holds no version.
pub fn workspace_version(root: &Path) -> Result<String, Error> {
    let path = root.join("Cargo.toml");
    let text = fs::read_to_string(&path).map_err(io(&path))?;
    text.parse::<toml::Table>()
        .ok()
        .and_then(|manifest| {
            manifest
                .get("workspace")?
                .get("package")?
                .get("version")?
                .as_str()
                .map(str::to_owned)
        })
        .ok_or(Error::NoWorkspaceVersion)
}

/// The version a build stamps on its artifacts.
///
/// The workspace's, or the one asked for when it is the workspace's or a
/// `+nightly.` stamp on it. Anything else would publish wrongly versioned artifacts under a
/// plausible name.
///
/// # Errors
///
/// When the version asked for is neither.
pub fn stamp(workspace: &str, asked: Option<&str>) -> Result<String, Error> {
    let Some(asked) = asked else {
        return Ok(workspace.to_owned());
    };
    let nightly = asked
        .strip_prefix(workspace)
        .and_then(|rest| rest.strip_prefix("+nightly."))
        .is_some_and(|stamp| !stamp.is_empty());
    if asked == workspace || nightly {
        Ok(asked.to_owned())
    } else {
        Err(Error::BadVersion {
            workspace: workspace.to_owned(),
            asked: asked.to_owned(),
        })
    }
}

/// A module's one-line `export const NAME = '<value>';`.
#[must_use]
pub fn js_const<'a>(text: &'a str, name: &str) -> Option<&'a str> {
    let before = format!("export const {name} = '");
    text.lines()
        .find_map(|line| line.strip_prefix(before.as_str())?.strip_suffix("';"))
}

/// The commit an install module's `REF` pins.
///
/// # Errors
///
/// When the module cannot be read or has no `REF`.
pub fn sdk_ref(root: &Path, module: &str) -> Result<String, Error> {
    module_const(root, module, "REF")
}

/// An install module's `export const <name> = '<value>';`, which must not be
/// empty.
///
/// # Errors
///
/// When the module cannot be read or has no such line.
pub fn module_const(root: &Path, module: &str, name: &'static str) -> Result<String, Error> {
    let path = root.join(module);
    let text = fs::read_to_string(&path).map_err(io(&path))?;
    js_const(&text, name)
        .filter(|value| !value.is_empty())
        .map(str::to_owned)
        .ok_or_else(|| Error::NoPin {
            module: module.to_owned(),
            name,
        })
}

/// Where the builders keep the SDKs they download, between runs:
/// `$XDG_CACHE_HOME/rusty-photon-pkg`, or `~/.cache/rusty-photon-pkg`, and
/// on Windows `%LOCALAPPDATA%\rusty-photon-pkg`.
#[must_use]
pub fn cache_dir() -> PathBuf {
    if cfg!(windows) {
        return std::env::var_os("LOCALAPPDATA")
            .map_or_default(PathBuf::from)
            .join("rusty-photon-pkg");
    }
    std::env::var_os("XDG_CACHE_HOME")
        .filter(|dir| !dir.is_empty())
        .map_or_else(
            || {
                std::env::var_os("HOME")
                    .map_or_default(PathBuf::from)
                    .join(".cache")
            },
            PathBuf::from,
        )
        .join("rusty-photon-pkg")
}

/// Downloads `url` to `dest` through a `.part` file, so an interrupted
/// download leaves nothing at `dest` for a later run to trust.
///
/// # Errors
///
/// When curl fails or the file cannot be moved into place.
pub fn download(runner: &mut impl Runner, url: &str, dest: &Path) -> Result<(), Error> {
    println!("Downloading {url}");
    let mut part = dest.as_os_str().to_owned();
    part.push(".part");
    let part = PathBuf::from(part);
    let part_arg = part.to_string_lossy();
    runner.run(&Cmd::new("curl", &["-fsSL", "-o", &part_arg, url]))?;
    fs::rename(&part, dest).map_err(io(dest))
}

/// Downloads `url` to `path` unless a previous run has.
///
/// # Errors
///
/// When it is not there and `offline` forbids the download, or the download
/// fails.
pub fn fetch(runner: &mut impl Runner, url: &str, path: &Path, offline: bool) -> Result<(), Error> {
    if path.is_file() {
        return Ok(());
    }
    if offline {
        return Err(missing(path));
    }
    download(runner, url, path)
}

/// The error for an SDK a `--skip-sdk-staging` run needs and no previous run
/// cached at `path`.
#[must_use]
pub fn missing(path: &Path) -> Error {
    Error::Io {
        path: path.display().to_string(),
        source: std::io::Error::new(
            std::io::ErrorKind::NotFound,
            "--skip-sdk-staging set, but a previous run has not cached it",
        ),
    }
}

/// Unpacks the zip at `zip` into `dest` with PowerShell's `Expand-Archive`.
///
/// Windows' own `tar.exe` fails on a name the code page cannot hold, as
/// ZWO's SDKs have (docs/skills/ci-logic.md, "An action that installs a
/// tool"). The paths reach PowerShell as variables, so nothing in them is
/// read as code.
///
/// # Errors
///
/// When PowerShell cannot start or the archive cannot be unpacked.
pub fn expand_zip(runner: &mut impl Runner, zip: &Path, dest: &Path) -> Result<(), Error> {
    let script = "$ErrorActionPreference = 'Stop'; $ProgressPreference = 'SilentlyContinue'; \
                  Expand-Archive -LiteralPath $env:RP_ZIP -DestinationPath $env:RP_DEST -Force";
    let cmd = Cmd::new(
        "powershell",
        &["-NoProfile", "-NonInteractive", "-Command", script],
    )
    .env("RP_ZIP", &zip.to_string_lossy())
    .env("RP_DEST", &dest.to_string_lossy());
    Ok(runner.run(&cmd)?)
}

/// Unpacks a QHYCCD SDK `archive` in `cache` and returns where:
/// `cache/<extract>`.
///
/// Unless a previous run unpacked it there, it downloads the archive, unless
/// that is cached, checks it has the SHA-256 `sha256` and unpacks it, a
/// `.zip` with [`expand_zip`] and a `.tar.gz` with tar.
///
/// # Errors
///
/// When it must be downloaded and `offline` forbids it, or the download or
/// the SHA-256 is bad.
pub fn unpack_qhy(
    runner: &mut impl Runner,
    cache: &Path,
    archive: &str,
    sha256: &str,
    extract: &str,
    offline: bool,
) -> Result<PathBuf, Error> {
    let extract = cache.join(extract);
    if extract.is_dir() {
        return Ok(extract);
    }
    if offline {
        return Err(missing(&extract));
    }
    let path = cache.join(archive);
    fetch(runner, &qhy::url(archive), &path, false)?;
    check_sha256(&path, sha256)?;
    let work = tempfile::tempdir_in(cache).map_err(io(cache))?;
    // QHYCCD's archives unpack to a directory named as they are.
    let unpacked = if let Some(name) = archive.strip_suffix(".zip") {
        expand_zip(runner, &path, work.path())?;
        work.path().join(name)
    } else {
        let work_arg = work.path().to_string_lossy();
        let path_arg = path.to_string_lossy();
        runner.run(&Cmd::new("tar", &["-xzf", &path_arg, "-C", &work_arg]))?;
        work.path().join(archive.trim_end_matches(".tar.gz"))
    };
    fs::rename(&unpacked, &extract).map_err(io(&unpacked))?;
    Ok(extract)
}

/// Stages a QHYCCD SDK `archive` in `cache`, as [`unpack_qhy`] does, and
/// returns the directory that holds its `libqhyccd.a`.
///
/// # Errors
///
/// When [`unpack_qhy`] fails, or the archive holds no `libqhyccd.a`.
pub fn stage_qhy(
    runner: &mut impl Runner,
    cache: &Path,
    archive: &str,
    sha256: &str,
    extract: &str,
    offline: bool,
) -> Result<PathBuf, Error> {
    let extract = unpack_qhy(runner, cache, archive, sha256, extract, offline)?;
    // Found rather than assumed: the archive's layout is QHYCCD's.
    let lib = find(&extract, "libqhyccd.a")?.ok_or_else(|| Error::Io {
        path: extract.display().to_string(),
        source: std::io::Error::new(std::io::ErrorKind::NotFound, "no libqhyccd.a under it"),
    })?;
    let dir = lib.parent().map_or_default(Path::to_path_buf);
    println!(
        "QHYCCD SDK {} staged: QHYCCD_SDK_DIR={}",
        qhy::VERSION,
        dir.display()
    );
    Ok(dir)
}

/// The first file named `name` under `dir`, in sorted order.
fn find(dir: &Path, name: &str) -> Result<Option<PathBuf>, Error> {
    let mut entries: Vec<PathBuf> = fs::read_dir(dir)
        .map_err(io(dir))?
        .filter_map(Result::ok)
        .map(|entry| entry.path())
        .collect();
    entries.sort();
    for path in entries {
        if path.is_dir() {
            if let Some(found) = find(&path, name)? {
                return Ok(Some(found));
            }
        } else if path.file_name().is_some_and(|file| file == name) {
            return Ok(Some(path));
        }
    }
    Ok(None)
}

/// The ZWO SDKs the selected services link, by library name: ASI's for
/// zwo-camera and EAF's for zwo-focuser (ADR-014).
#[must_use]
pub fn zwo_sdks(services: &[String]) -> Vec<&'static str> {
    let mut sdks = Vec::new();
    if services.iter().any(|service| service == "zwo-camera") {
        sdks.push("libASICamera2");
    }
    if services.iter().any(|service| service == "zwo-focuser") {
        sdks.push("libEAFFocuser");
    }
    sdks
}

/// The cargo runs that build `services`, as `-p` arguments.
///
/// The zwo services build each alone, as cargo unifies features within a run
/// and would link both binaries to every ZWO SDK, and the rest together,
/// with doctor when sentinel is among them, since sentinel's package carries
/// doctor.
#[must_use]
pub fn cargo_runs(services: &[String]) -> Vec<Vec<String>> {
    let mut batched: Vec<String> = Vec::new();
    let mut alone: Vec<Vec<String>> = Vec::new();
    for service in services {
        if service == "zwo-camera" || service == "zwo-focuser" {
            alone.push(vec!["-p".to_owned(), service.clone()]);
        } else {
            batched.extend(["-p".to_owned(), service.clone()]);
        }
    }
    if services.iter().any(|service| service == "sentinel") {
        batched.extend(["-p".to_owned(), "doctor".to_owned()]);
    }
    let mut runs = Vec::new();
    if !batched.is_empty() {
        runs.push(batched);
    }
    runs.extend(alone);
    runs
}

/// `cmd`, a cargo run, with `flags` as the only extra flags rustc gets.
///
/// They go in `RUSTFLAGS`, which replaces any the caller has, so the
/// binaries do not depend on the shell they were built from; and
/// `CARGO_ENCODED_RUSTFLAGS`, which Cargo reads before `RUSTFLAGS`, is
/// taken out.
#[must_use]
pub fn rustflags(cmd: Cmd, flags: &str) -> Cmd {
    cmd.env_remove(&["CARGO_ENCODED_RUSTFLAGS".into()])
        .env("RUSTFLAGS", flags)
}

/// The binaries the cargo runs build for `services`: theirs, and doctor's
/// with sentinel's.
#[must_use]
pub fn binaries(services: &[String]) -> Vec<String> {
    let mut binaries = services.to_vec();
    if services.iter().any(|service| service == "sentinel") {
        binaries.push("doctor".to_owned());
    }
    binaries
}

/// `bytes` in lowercase hex.
#[must_use]
pub fn hex(bytes: &[u8]) -> String {
    let mut hex = String::new();
    for byte in bytes {
        // Writing to a String cannot fail.
        let _ = write!(hex, "{byte:02x}");
    }
    hex
}

/// The SHA-256 of the file at `path`, in hex.
///
/// # Errors
///
/// When it cannot be read.
pub fn sha256(path: &Path) -> Result<String, Error> {
    let bytes = fs::read(path).map_err(io(path))?;
    Ok(hex(Sha256::digest(&bytes).as_slice()))
}

/// Fails unless the file at `path` has the SHA-256 `expected`.
///
/// # Errors
///
/// When it cannot be read, or its hash is another.
pub fn check_sha256(path: &Path, expected: &str) -> Result<(), Error> {
    let actual = sha256(path)?;
    if actual == expected {
        Ok(())
    } else {
        Err(Error::Sha256Mismatch {
            path: path.display().to_string(),
            expected: expected.to_owned(),
            actual,
        })
    }
}

/// Writes `dir/SHA256SUMS.txt` for every file in `dir` whose name ends with
/// one of `suffixes`, sorted by name, one `<sha256>  <name>` line each, as
/// `sha256sum` and `shasum -a 256` print them.
///
/// # Errors
///
/// When the directory or a file cannot be read, or the manifest written.
pub fn write_sums(dir: &Path, suffixes: &[&str]) -> Result<(), Error> {
    let mut names: Vec<String> = fs::read_dir(dir)
        .map_err(io(dir))?
        .filter_map(Result::ok)
        .filter_map(|entry| entry.file_name().into_string().ok())
        .filter(|name| suffixes.iter().any(|suffix| name.ends_with(suffix)))
        .collect();
    names.sort();
    let mut sums = String::new();
    for name in &names {
        let _ = writeln!(sums, "{}  {name}", sha256(&dir.join(name))?);
    }
    let path = dir.join("SHA256SUMS.txt");
    fs::write(&path, sums).map_err(io(&path))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::process::Fake;

    #[test]
    fn asks_git_for_the_repository_root() {
        let mut runner = Fake::answering(&["/repo\n"]);
        assert_eq!(repo_root(&mut runner).unwrap(), PathBuf::from("/repo"));
        assert_eq!(runner.lines(), ["git rev-parse --show-toplevel"]);
    }

    #[test]
    fn stamps_the_workspace_version_or_a_nightly_on_it() {
        assert_eq!(stamp("0.1.0", None).unwrap(), "0.1.0");
        assert_eq!(stamp("0.1.0", Some("0.1.0")).unwrap(), "0.1.0");
        let nightly = "0.1.0+nightly.202610220507.gabc1234";
        assert_eq!(stamp("0.1.0", Some(nightly)).unwrap(), nightly);
    }

    #[test]
    fn refuses_any_other_version() {
        for asked in [
            "0.1.1",
            "0.1.0+nightly.",
            "0.1.0-rc1",
            "0.1.0+other.1",
            "1.0#{x}",
        ] {
            let error = stamp("0.1.0", Some(asked)).unwrap_err();
            assert_eq!(
                error.to_string(),
                format!("--version must be 0.1.0 or 0.1.0+nightly.<stamp> (got: {asked})")
            );
        }
    }

    #[test]
    fn reads_the_workspace_version() {
        let root = tempfile::tempdir().unwrap();
        let manifest = "[workspace]\nmembers = []\n\n[workspace.package]\nversion = \"0.2.0\"\n";
        fs::write(root.path().join("Cargo.toml"), manifest).unwrap();
        assert_eq!(workspace_version(root.path()).unwrap(), "0.2.0");
        fs::write(
            root.path().join("Cargo.toml"),
            "[workspace]\nmembers = []\n",
        )
        .unwrap();
        assert!(matches!(
            workspace_version(root.path()),
            Err(Error::NoWorkspaceVersion)
        ));
    }

    #[test]
    fn reads_an_install_modules_ref() {
        let root = tempfile::tempdir().unwrap();
        let module = root.path().join(ZWO_MODULE);
        fs::create_dir_all(module.parent().unwrap()).unwrap();
        fs::write(&module, "// x\nexport const REF = 'b0802f2';\n").unwrap();
        assert_eq!(sdk_ref(root.path(), ZWO_MODULE).unwrap(), "b0802f2");
        fs::write(&module, "export const REF = '';\n").unwrap();
        let error = sdk_ref(root.path(), ZWO_MODULE).unwrap_err();
        assert!(matches!(error, Error::NoPin { name: "REF", .. }), "{error}");
    }

    #[test]
    fn the_install_modules_pin_a_commit() {
        let root = crate::repo_root();
        for module in [ZWO_MODULE, SVBONY_MODULE] {
            let commit = sdk_ref(&root, module).unwrap();
            assert_eq!(commit.len(), 40, "{module}: {commit}");
        }
    }

    /// `cargo ci build-msi` downloads ZWO's SDKs for Windows from the links
    /// install-zwo-sdk's module holds.
    #[test]
    fn the_zwo_module_holds_the_windows_links() {
        let root = crate::repo_root();
        for name in ["CAMERA_SDK_URL", "EAF_SDK_URL"] {
            let url = module_const(&root, ZWO_MODULE, name).unwrap();
            assert!(url.starts_with("https://dl.zwoastro.com/"), "{name}: {url}");
        }
    }

    #[test]
    fn selects_every_packaged_service_but_svbony_camera_or_the_ones_asked_for() {
        let packaged = ["filemonitor", "svbony-camera", "zwo-camera"].map(str::to_owned);
        assert_eq!(
            macos_services(&packaged, None).unwrap(),
            ["filemonitor", "zwo-camera"]
        );
        let asked = ["zwo-camera", "filemonitor"].map(str::to_owned);
        assert_eq!(macos_services(&packaged, Some(&asked)).unwrap(), asked);
        let error = macos_services(&packaged, Some(&["doctor".to_owned()])).unwrap_err();
        assert_eq!(
            error.to_string(),
            "unknown or unpackaged service: doctor (packaged: filemonitor svbony-camera zwo-camera)"
        );
        let error = macos_services(&packaged, Some(&["svbony-camera".to_owned()])).unwrap_err();
        assert!(matches!(error, Error::NoMacosSdk), "{error}");
    }

    #[test]
    fn finds_the_services_with_a_pkg_directory() {
        let root = tempfile::tempdir().unwrap();
        for service in ["zwo-camera", "filemonitor"] {
            fs::create_dir_all(root.path().join("services").join(service).join("pkg")).unwrap();
        }
        fs::create_dir_all(root.path().join("services/doctor")).unwrap();
        assert_eq!(
            packaged(root.path()).unwrap(),
            ["filemonitor", "zwo-camera"]
        );
    }

    #[test]
    fn every_packaged_service_has_a_port() {
        let packaged = packaged(&crate::repo_root()).unwrap();
        let missing: Vec<&String> = packaged.iter().filter(|svc| port(svc).is_none()).collect();
        assert!(
            missing.is_empty(),
            "no port in packaging::PORTS: {missing:?}"
        );
        let mut ports: Vec<u16> = PORTS.iter().map(|&(_, port)| port).collect();
        ports.sort_unstable();
        ports.dedup();
        assert_eq!(ports.len(), PORTS.len(), "two services share a port");
    }

    #[test]
    fn downloads_through_a_part_file() {
        let dir = tempfile::tempdir().unwrap();
        let dest = dir.path().join("sdk.tar.gz");
        fs::write(dir.path().join("sdk.tar.gz.part"), "bytes").unwrap();
        let mut runner = Fake::default();
        download(&mut runner, "https://example.com/sdk.tar.gz", &dest).unwrap();
        assert_eq!(
            runner.lines(),
            [format!(
                "curl -fsSL -o {} https://example.com/sdk.tar.gz",
                dir.path().join("sdk.tar.gz.part").display()
            )]
        );
        assert_eq!(fs::read_to_string(&dest).unwrap(), "bytes");
    }

    #[test]
    fn a_failed_download_leaves_nothing_in_place() {
        let dir = tempfile::tempdir().unwrap();
        let dest = dir.path().join("sdk.tar.gz");
        let mut runner = Fake {
            fail_at: Some(0),
            ..Fake::default()
        };
        download(&mut runner, "https://example.com/sdk.tar.gz", &dest).unwrap_err();
        assert!(!dest.exists());
    }

    #[test]
    fn checks_a_files_sha256() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("a");
        fs::write(&path, "abc").unwrap();
        let abc = "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad";
        check_sha256(&path, abc).unwrap();
        let error = check_sha256(&path, &"0".repeat(64)).unwrap_err();
        assert!(matches!(error, Error::Sha256Mismatch { .. }), "{error}");
    }

    #[test]
    fn writes_the_checksum_manifest_of_the_files_with_the_suffixes() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("b.deb"), "b").unwrap();
        fs::write(dir.path().join("a.rpm"), "abc").unwrap();
        fs::write(dir.path().join("notes.txt"), "x").unwrap();
        write_sums(dir.path(), &[".deb", ".rpm"]).unwrap();
        let b = hex(Sha256::digest(b"b").as_slice());
        assert_eq!(
            fs::read_to_string(dir.path().join("SHA256SUMS.txt")).unwrap(),
            format!(
                "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad  a.rpm\n{b}  b.deb\n"
            )
        );
    }

    /// The Windows archive is a zip, which Expand-Archive unpacks.
    #[test]
    fn unpacks_a_qhy_zip_with_expand_archive() {
        let cache = tempfile::tempdir().unwrap();
        let sha256 = hex(Sha256::digest(b"zip").as_slice());
        let mut runner = crate::process::Script::default()
            .on_cmd("curl", |cmd| {
                fs::write(&cmd.args[2], "zip").unwrap();
                Ok(String::new())
            })
            .on_cmd("powershell", |cmd| {
                let dest = &cmd
                    .env
                    .iter()
                    .find(|(name, _)| name == "RP_DEST")
                    .unwrap()
                    .1;
                let lib = Path::new(dest).join("sdk_win64_26.06.04/x64");
                fs::create_dir_all(&lib).unwrap();
                fs::write(lib.join("qhyccd.lib"), "lib").unwrap();
                Ok(String::new())
            });
        let archive = "sdk_win64_26.06.04.zip";
        let extract = unpack_qhy(
            &mut runner,
            cache.path(),
            archive,
            &sha256,
            "qhy-win64",
            false,
        )
        .unwrap();
        assert_eq!(extract, cache.path().join("qhy-win64"));
        assert!(extract.join("x64/qhyccd.lib").is_file());
        let programs: Vec<&str> = runner
            .lines
            .iter()
            .map(|line| line.split(' ').next().unwrap())
            .collect();
        assert_eq!(programs, ["curl", "powershell"]);
        // Unpacked once: a later run takes it as it is.
        unpack_qhy(
            &mut runner,
            cache.path(),
            archive,
            &sha256,
            "qhy-win64",
            false,
        )
        .unwrap();
        assert_eq!(runner.lines.len(), 2);
    }

    #[test]
    fn fetches_only_what_is_not_cached_and_nothing_offline() {
        let dir = tempfile::tempdir().unwrap();
        let cached = dir.path().join("cached.so");
        fs::write(&cached, "blob").unwrap();
        let mut runner = Fake::default();
        fetch(&mut runner, "https://example.com/x", &cached, true).unwrap();
        assert!(runner.ran.is_empty(), "{:?}", runner.lines());
        let absent = dir.path().join("absent.so");
        let error = fetch(&mut runner, "https://example.com/x", &absent, true).unwrap_err();
        assert!(
            error.to_string().contains("--skip-sdk-staging set"),
            "{error}"
        );
        assert!(runner.ran.is_empty(), "{:?}", runner.lines());
    }

    #[test]
    fn builds_each_zwo_service_alone_and_doctor_with_sentinel() {
        let services = ["zwo-focuser", "rp", "sentinel", "zwo-camera"].map(str::to_owned);
        assert_eq!(
            cargo_runs(&services),
            [
                vec!["-p", "rp", "-p", "sentinel", "-p", "doctor"],
                vec!["-p", "zwo-focuser"],
                vec!["-p", "zwo-camera"],
            ]
        );
        assert_eq!(
            binaries(&services),
            ["zwo-focuser", "rp", "sentinel", "zwo-camera", "doctor"]
        );
        assert_eq!(zwo_sdks(&services), ["libASICamera2", "libEAFFocuser"]);
        let alone = ["zwo-camera".to_owned()];
        assert_eq!(cargo_runs(&alone), [vec!["-p", "zwo-camera"]]);
        assert_eq!(zwo_sdks(&["rp".to_owned()]), Vec::<&str>::new());
    }

    /// The archives QHYCCD publishes for the pinned release, which the
    /// builders download and unpack.
    #[test]
    fn names_the_qhy_archives_as_qhyccd_does() {
        assert_eq!(
            qhy::mac_archive(),
            format!("sdk_mac_arm_{}.tar.gz", qhy::VERSION)
        );
        assert_eq!(
            qhy::linux_x86_64_archive(),
            format!("sdk_linux64_{}.tar.gz", qhy::VERSION)
        );
        assert_eq!(
            qhy::linux_aarch64_archive(),
            format!("sdk_linux_arm64_{}.tar.gz", qhy::VERSION)
        );
    }

    #[test]
    fn puts_the_qhy_sdk_in_its_dotless_directory() {
        assert_eq!(
            qhy::url("sdk_mac_arm_26.06.04.tar.gz"),
            "https://www.qhyccd.com/file/repository/publish/SDK/260604/sdk_mac_arm_26.06.04.tar.gz"
        );
    }
}
