//! `cargo ci build-packages`: builds the services' .deb packages, and with
//! `--rpm` their .rpm packages, into `dist/<version>/`.
//!
//! It builds natively on the Linux machine they are for: a hosted runner of
//! each architecture in the nightly, or the rig or a dev box on demand
//! (docs/packaging.md). A `SHA256SUMS.txt` goes with them.
//!
//! 1. It installs what the build needs, with apt where there is apt, and
//!    cargo-deb, and cargo-generate-rpm with `--rpm`.
//! 2. It stages the native SDKs in the cache ([`packaging::cache_dir`]):
//!    QHYCCD's static `libqhyccd.a`, which qhy-camera links; the ZWO blob
//!    each zwo service links, ASI's for zwo-camera and EAF's for zwo-focuser
//!    (ADR-014), which it also copies into the service's `pkg/lib/`, the
//!    package's payload (ADR-013), so the zwo packages share no file; and
//!    `SVBony`'s, which svbony-camera links and its package never ships, as
//!    `SVBony` grants no license to (ADR-018): the operator installs it on
//!    the target with the helper the package carries. The ZWO and `SVBony`
//!    blobs carry no SONAME, so the binaries load them by name, through
//!    [`RUNPATH`].
//! 3. It release-builds the services with that one RUNPATH, harmless where
//!    unused, each zwo service in a cargo run of its own
//!    ([`packaging::cargo_runs`]), and strips them.
//! 4. It packages each service with `cargo deb --no-build --no-strip`, and
//!    with `--rpm` `cargo generate-rpm`: a rebuild would drop the SDKs'
//!    environment and the RUNPATH.

use std::fs;
use std::path::{Path, PathBuf};

use crate::packaging::{self, io, qhy, Error};
use crate::process::{Cmd, Runner};

/// Where the packages put the ZWO blobs, and the `SVBony` helper the
/// operator's SDK, which the binaries find through their RUNPATH. It is off
/// ldconfig's path on purpose (ADR-013).
pub const RUNPATH: &str = "/usr/lib/rusty-photon";

/// What the build needs from apt: libusb and libudev, which the camera SDKs
/// link; clang and libclang, for bindgen in libzwo-sys; and dpkg-dev, whose
/// `dpkg-shlibdeps` cargo-deb runs to work out each package's dependencies.
const APT_PACKAGES: &[&str] = &[
    "build-essential",
    "pkg-config",
    "curl",
    "git",
    "ca-certificates",
    "dpkg-dev",
    "libusb-1.0-0-dev",
    "libudev-dev",
    "clang",
    "libclang-dev",
];

/// What a build makes.
#[derive(Clone, Debug, Default)]
pub struct Request {
    /// Only these services. `None` builds every packaged one.
    pub services: Option<Vec<String>>,
    /// The .rpm packages too.
    pub rpm: bool,
    /// Download nothing: rebuild from the SDKs a previous run cached.
    pub skip_sdk_staging: bool,
    /// The version to stamp: the workspace's when `None`, or a nightly stamp
    /// on it.
    pub version: Option<String>,
}

/// The two Linux architectures packages are built for.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Arch {
    X86_64,
    Aarch64,
}

impl Arch {
    /// As `uname -m` names it, which the cache's directories carry.
    const fn uname(self) -> &'static str {
        match self {
            Self::X86_64 => "x86_64",
            Self::Aarch64 => "aarch64",
        }
    }

    /// indi-3rdparty's directory for its ZWO blobs.
    const fn zwo(self) -> &'static str {
        match self {
            Self::X86_64 => "x64",
            Self::Aarch64 => "armv8",
        }
    }

    /// indi-3rdparty's suffix for its `SVBony` blob, which is not the ZWO
    /// directory's name.
    const fn svbony(self) -> &'static str {
        match self {
            Self::X86_64 => "amd64",
            Self::Aarch64 => "armv8",
        }
    }

    /// QHYCCD's archive and its SHA-256.
    fn qhy(self) -> (String, &'static str) {
        match self {
            Self::X86_64 => (qhy::linux_x86_64_archive(), qhy::SHA256_LINUX_X86_64),
            Self::Aarch64 => (qhy::linux_aarch64_archive(), qhy::SHA256_LINUX_AARCH64),
        }
    }
}

/// What a build needs to know of the machine it runs on, so the tests can
/// stand in for one.
#[derive(Clone, Debug)]
pub struct Host {
    pub arch: Arch,
    /// Whether `apt-get` is on `PATH`. Without it, the build names what it
    /// needs and goes on.
    pub apt: bool,
    /// [`packaging::cache_dir`].
    pub cache: PathBuf,
}

impl Host {
    /// This machine.
    #[must_use]
    pub fn this(arch: Arch) -> Self {
        let apt = std::env::var_os("PATH").is_some_and(|path| {
            std::env::split_paths(&path).any(|dir| dir.join("apt-get").is_file())
        });
        Self {
            arch,
            apt,
            cache: packaging::cache_dir(),
        }
    }
}

/// The architecture to build for: this machine's, which must be a Linux
/// one packages are built for.
///
/// # Errors
///
/// On another OS or architecture.
pub fn check_host(os: &str, arch: &str) -> Result<Arch, String> {
    if os != "linux" {
        return Err(
            "deb and rpm packages are built and verified on Linux only (macOS ships tarballs — \
             `cargo ci build-tarballs`; Windows an MSI — scripts/build-msi.ps1)"
                .to_owned(),
        );
    }
    match arch {
        "x86_64" => Ok(Arch::X86_64),
        "aarch64" => Ok(Arch::Aarch64),
        _ => Err(format!(
            "unsupported architecture {arch} (need x86_64 or aarch64)"
        )),
    }
}

/// The environment the release builds need, beyond the RUNPATH.
#[derive(Debug, Default)]
struct Staged {
    /// `QHYCCD_SDK_DIR`: where `libqhyccd.a` is.
    qhy: Option<PathBuf>,
    /// `ZWO_SDK_LIB_DIR`: the ZWO blobs.
    zwo: Option<PathBuf>,
    /// `SVBONY_SDK_LIB_DIR`: the `SVBony` blob, for the link only.
    svbony: Option<PathBuf>,
}

/// Installs what the build needs that the machine lacks.
fn prerequisites(runner: &mut impl Runner, host: &Host, rpm: bool) -> Result<(), Error> {
    if host.apt {
        let missing: Vec<&str> = APT_PACKAGES
            .iter()
            .copied()
            .filter(|package| {
                !runner
                    .code(&Cmd::new("dpkg", &["-s", package]))
                    .is_ok_and(|code| code == Some(0))
            })
            .collect();
        if !missing.is_empty() {
            println!("Installing build prerequisites: {}", missing.join(" "));
            let root = runner.read(&Cmd::new("id", &["-u"]))?.trim_end() == "0";
            let apt_get = |args: &[&str]| {
                if root {
                    Cmd::new("apt-get", args)
                } else {
                    Cmd::new("sudo", &[&["apt-get"], args].concat())
                }
            };
            runner.run(&apt_get(&["update", "-qq"]))?;
            let install = [
                &["install", "-y", "--no-install-recommends"],
                missing.as_slice(),
            ]
            .concat();
            runner.run(&apt_get(&install))?;
        }
    } else {
        eprintln!(
            "build-packages: apt-get not found; make sure equivalents of these are installed: {}",
            APT_PACKAGES.join(" ")
        );
    }
    let subcommands: &[&str] = if rpm {
        &["deb", "generate-rpm"]
    } else {
        &["deb"]
    };
    for subcommand in subcommands {
        // Asked as the build will run it: through cargo, which knows it
        // when its binary is on `PATH`.
        let installed = runner
            .code(&Cmd::new("cargo", &[subcommand, "--version"]))
            .is_ok_and(|code| code == Some(0));
        if !installed {
            let tool = format!("cargo-{subcommand}");
            runner.run(&Cmd::new("cargo", &["install", "--locked", &tool]))?;
        }
    }
    Ok(())
}

/// Stages the ZWO blobs the services link in the cache, which is their link
/// path, and copies each service's own into its `pkg/lib/`, which then
/// holds nothing else of ZWO's.
fn stage_zwo(
    runner: &mut impl Runner,
    root: &Path,
    host: &Host,
    services: &[String],
    offline: bool,
) -> Result<PathBuf, Error> {
    let commit = packaging::sdk_ref(root, packaging::ZWO_MODULE)?;
    let dir = host.cache.join(format!("zwo-{commit}-{}", host.arch.zwo()));
    fs::create_dir_all(&dir).map_err(io(&dir))?;
    let blobs = packaging::zwo_sdks(services);
    for blob in &blobs {
        let url = format!(
            "https://github.com/indilib/indi-3rdparty/raw/{commit}/libasi/{}/{blob}.bin",
            host.arch.zwo()
        );
        packaging::fetch(runner, &url, &dir.join(format!("{blob}.so")), offline)?;
    }
    println!(
        "ZWO SDK blobs (indi-3rdparty {commit}) staged: {}; ZWO_SDK_LIB_DIR={}",
        blobs.join(" "),
        dir.display()
    );
    for service in services {
        let Some(own) = packaging::zwo_sdks(std::slice::from_ref(service))
            .first()
            .copied()
        else {
            continue;
        };
        let lib = root.join("services").join(service).join("pkg/lib");
        fs::create_dir_all(&lib).map_err(io(&lib))?;
        for other in ["libASICamera2", "libEAFFocuser", "libEFWFilter"] {
            let path = lib.join(format!("{other}.so"));
            if other != own && path.exists() {
                fs::remove_file(&path).map_err(io(&path))?;
            }
        }
        let from = dir.join(format!("{own}.so"));
        fs::copy(&from, lib.join(format!("{own}.so"))).map_err(io(&from))?;
    }
    Ok(dir)
}

/// Stages `SVBony`'s blob in the cache, under the name the link looks for,
/// and returns its directory. svbony-camera's package never ships it.
fn stage_svbony(
    runner: &mut impl Runner,
    root: &Path,
    host: &Host,
    offline: bool,
) -> Result<PathBuf, Error> {
    let commit = packaging::sdk_ref(root, packaging::SVBONY_MODULE)?;
    let dir = host
        .cache
        .join(format!("svbony-{commit}-{}", host.arch.uname()));
    fs::create_dir_all(&dir).map_err(io(&dir))?;
    let url = format!(
        "https://github.com/indilib/indi-3rdparty/raw/{commit}/libsvbony/libSVBCameraSDK_{}.bin",
        host.arch.svbony()
    );
    packaging::fetch(runner, &url, &dir.join("libSVBCameraSDK.so"), offline)?;
    println!(
        "SVBony SDK (indi-3rdparty {commit}) staged: SVBONY_SDK_LIB_DIR={}",
        dir.display()
    );
    Ok(dir)
}

fn cargo_build(staged: &Staged, root: &Path, packages: &[String]) -> Cmd {
    let mut args = vec!["build", "--release"];
    args.extend(packages.iter().map(String::as_str));
    // It replaces any RUSTFLAGS the caller has, so the binaries do not
    // depend on the shell they were built from.
    let mut cmd = Cmd::new("cargo", &args)
        .dir(root)
        .env("RUSTFLAGS", &format!("-C link-arg=-Wl,-rpath,{RUNPATH}"));
    if let Some(dir) = &staged.qhy {
        cmd = cmd.env("QHYCCD_SDK_DIR", &dir.to_string_lossy());
    }
    if let Some(dir) = &staged.zwo {
        cmd = cmd.env("ZWO_SDK_LIB_DIR", &dir.to_string_lossy());
    }
    if let Some(dir) = &staged.svbony {
        cmd = cmd.env("SVBONY_SDK_LIB_DIR", &dir.to_string_lossy());
    }
    cmd
}

/// The rpm version for a nightly stamp: rpm's `^` marks a snapshot after
/// its base release and before anything else, which `+` does not. `None`
/// for a release, which the rpm takes from `Cargo.toml`.
fn rpm_version(version: &str) -> Option<String> {
    version
        .contains("+nightly.")
        .then(|| version.replacen("+nightly.", "^", 1))
}

/// Packages `service` into `dist`.
fn package(
    runner: &mut impl Runner,
    root: &Path,
    request: &Request,
    service: &str,
    dist: &Path,
) -> Result<(), Error> {
    println!("Packaging {service}");
    let out = format!("{}/", dist.display());
    let mut deb = vec!["deb", "-p", service, "--no-build", "--no-strip"];
    // cargo-deb takes a version as it is given, with no `-1` revision.
    if let Some(version) = &request.version {
        deb.extend(["--deb-version", version]);
    }
    deb.extend(["--output", &out]);
    runner.run(&Cmd::new("cargo", &deb).dir(root))?;
    if request.rpm {
        let manifest = format!("services/{service}");
        let mut rpm = vec!["generate-rpm", "-p", &manifest, "-o", &out];
        let metadata = request
            .version
            .as_deref()
            .and_then(rpm_version)
            .map(|version| format!("version = \"{version}\""));
        if let Some(metadata) = &metadata {
            rpm.extend(["--set-metadata", metadata]);
        }
        runner.run(&Cmd::new("cargo", &rpm).dir(root))?;
    }
    Ok(())
}

/// Builds the packages `request` asks for and returns the directory they are
/// in.
///
/// # Errors
///
/// When the version or a service is refused, an SDK cannot be staged, or a
/// command or file operation fails.
pub fn build(
    runner: &mut impl Runner,
    root: &Path,
    host: &Host,
    request: &Request,
) -> Result<PathBuf, Error> {
    let version = packaging::stamp(
        &packaging::workspace_version(root)?,
        request.version.as_deref(),
    )?;
    let packaged = packaging::packaged(root)?;
    let services = match &request.services {
        None => packaged,
        Some(asked) => {
            if let Some(unknown) = asked.iter().find(|service| !packaged.contains(service)) {
                return Err(Error::UnknownService {
                    service: unknown.clone(),
                    packaged: packaged.join(" "),
                });
            }
            asked.clone()
        }
    };

    prerequisites(runner, host, request.rpm)?;

    fs::create_dir_all(&host.cache).map_err(io(&host.cache))?;
    let offline = request.skip_sdk_staging;
    let has = |name: &str| services.iter().any(|service| service == name);
    let mut staged = Staged::default();
    if has("qhy-camera") {
        let (archive, sha256) = host.arch.qhy();
        let extract = format!("qhy-sdk-{}-{}", qhy::VERSION, host.arch.uname());
        staged.qhy = Some(packaging::stage_qhy(
            runner,
            &host.cache,
            &archive,
            sha256,
            &extract,
            offline,
        )?);
    }
    if !packaging::zwo_sdks(&services).is_empty() {
        staged.zwo = Some(stage_zwo(runner, root, host, &services, offline)?);
    }
    if has("svbony-camera") {
        staged.svbony = Some(stage_svbony(runner, root, host, offline)?);
    }

    for packages in packaging::cargo_runs(&services) {
        println!("Building release binaries: {}", packages.join(" "));
        runner.run(&cargo_build(&staged, root, &packages))?;
    }
    for binary in &packaging::binaries(&services) {
        let path = root.join("target/release").join(binary);
        runner.run(&Cmd::new("strip", &[&path.to_string_lossy()]))?;
    }

    let dist = root.join("dist").join(&version);
    fs::create_dir_all(&dist).map_err(io(&dist))?;
    for service in &services {
        package(runner, root, request, service, &dist)?;
    }
    packaging::write_sums(&dist, &[".deb", ".rpm"])?;
    Ok(dist)
}

// The tools build on Linux; their tests compare Unix paths.
#[cfg(test)]
#[cfg(unix)]
mod tests {
    use super::*;
    use crate::process::{Fake, Script};

    const ZWO_REF: &str = "b0802f28055b67aa6a99580d260c3bb4c27eba4b";
    const SVBONY_REF: &str = "cd50a3b95032d850cca28d8162513276bc1349ba";

    /// A repository and a Linux machine: `services` packaged, svbony-camera
    /// too, the install modules' pins, and a cache holding every SDK, on an
    /// `x86_64` host with apt.
    struct Fixture {
        root: tempfile::TempDir,
        host: Host,
        cache_dir: tempfile::TempDir,
    }

    fn fixture(services: &[&str]) -> Fixture {
        let root = tempfile::tempdir().unwrap();
        let r = root.path();
        fs::write(
            r.join("Cargo.toml"),
            "[workspace]\nmembers = []\n\n[workspace.package]\nversion = \"0.1.0\"\n",
        )
        .unwrap();
        for service in services.iter().chain(&["svbony-camera"]) {
            fs::create_dir_all(r.join("services").join(service).join("pkg")).unwrap();
        }
        for (module, commit) in [
            (packaging::ZWO_MODULE, ZWO_REF),
            (packaging::SVBONY_MODULE, SVBONY_REF),
        ] {
            let module = r.join(module);
            fs::create_dir_all(module.parent().unwrap()).unwrap();
            fs::write(&module, format!("export const REF = '{commit}';\n")).unwrap();
        }

        let cache_dir = tempfile::tempdir().unwrap();
        let cache = cache_dir.path().to_path_buf();
        let qhy = cache.join(format!(
            "qhy-sdk-{v}-x86_64/sdk_linux64_{v}/usr/local/lib",
            v = qhy::VERSION
        ));
        fs::create_dir_all(&qhy).unwrap();
        fs::write(qhy.join("libqhyccd.a"), "a").unwrap();
        let zwo = cache.join(format!("zwo-{ZWO_REF}-x64"));
        fs::create_dir_all(&zwo).unwrap();
        for blob in ["libASICamera2", "libEAFFocuser"] {
            fs::write(zwo.join(format!("{blob}.so")), blob).unwrap();
        }
        let svbony = cache.join(format!("svbony-{SVBONY_REF}-x86_64"));
        fs::create_dir_all(&svbony).unwrap();
        fs::write(svbony.join("libSVBCameraSDK.so"), "svb").unwrap();
        let host = Host {
            arch: Arch::X86_64,
            apt: true,
            cache,
        };
        Fixture {
            root,
            host,
            cache_dir,
        }
    }

    /// The commands, with the fixture's paths shortened to `<root>` and
    /// `<cache>`.
    fn lines(lines: &[String], fixture: &Fixture) -> Vec<String> {
        let root = fixture.root.path().display().to_string();
        let cache = fixture.cache_dir.path().display().to_string();
        lines
            .iter()
            .map(|line| line.replace(&root, "<root>").replace(&cache, "<cache>"))
            .collect()
    }

    const ENV: &str = "RUSTFLAGS=-C link-arg=-Wl,-rpath,/usr/lib/rusty-photon";

    #[test]
    fn builds_and_packages_every_packaged_service() {
        let fixture = fixture(&["filemonitor", "qhy-camera", "sentinel", "zwo-camera"]);
        let mut runner = Fake::default();
        let dist = build(
            &mut runner,
            fixture.root.path(),
            &fixture.host,
            &Request::default(),
        )
        .unwrap();
        assert_eq!(dist, fixture.root.path().join("dist/0.1.0"));
        let env = format!(
            "{ENV} QHYCCD_SDK_DIR=<cache>/qhy-sdk-{v}-x86_64/sdk_linux64_{v}/usr/local/lib \
             ZWO_SDK_LIB_DIR=<cache>/zwo-{ZWO_REF}-x64 \
             SVBONY_SDK_LIB_DIR=<cache>/svbony-{SVBONY_REF}-x86_64",
            v = qhy::VERSION
        );
        let deb = |svc: &str| {
            format!("cargo deb -p {svc} --no-build --no-strip --output <root>/dist/0.1.0/")
        };
        let lines = lines(&runner.lines(), &fixture);
        let dpkg: Vec<String> = APT_PACKAGES
            .iter()
            .map(|package| format!("dpkg -s {package}"))
            .collect();
        assert_eq!(lines[..dpkg.len()], dpkg);
        assert_eq!(
            lines[dpkg.len()..],
            [
                "cargo deb --version".to_owned(),
                format!(
                    "{env} cargo build --release -p filemonitor -p qhy-camera -p sentinel \
                     -p svbony-camera -p doctor"
                ),
                format!("{env} cargo build --release -p zwo-camera"),
                "strip <root>/target/release/filemonitor".to_owned(),
                "strip <root>/target/release/qhy-camera".to_owned(),
                "strip <root>/target/release/sentinel".to_owned(),
                "strip <root>/target/release/svbony-camera".to_owned(),
                "strip <root>/target/release/zwo-camera".to_owned(),
                "strip <root>/target/release/doctor".to_owned(),
                deb("filemonitor"),
                deb("qhy-camera"),
                deb("sentinel"),
                deb("svbony-camera"),
                deb("zwo-camera"),
            ]
        );
        assert!(dist.join("SHA256SUMS.txt").is_file());
    }

    /// Each zwo package's payload is its own blob, and nothing else of
    /// ZWO's; svbony-camera's is nothing (ADR-018).
    #[test]
    fn puts_each_zwo_services_own_blob_in_its_payload_and_svbonys_in_none() {
        let fixture = fixture(&["zwo-camera", "zwo-focuser"]);
        let camera_lib = fixture.root.path().join("services/zwo-camera/pkg/lib");
        fs::create_dir_all(&camera_lib).unwrap();
        fs::write(camera_lib.join("libEAFFocuser.so"), "stale").unwrap();
        fs::write(camera_lib.join("libEFWFilter.so"), "stale").unwrap();
        build(
            &mut Fake::default(),
            fixture.root.path(),
            &fixture.host,
            &Request::default(),
        )
        .unwrap();
        let listing = |service: &str| {
            let lib = fixture
                .root
                .path()
                .join("services")
                .join(service)
                .join("pkg/lib");
            let mut names: Vec<String> = fs::read_dir(lib)
                .map(|entries| {
                    entries
                        .map(|entry| entry.unwrap().file_name().into_string().unwrap())
                        .collect()
                })
                .unwrap_or_default();
            names.sort();
            names
        };
        assert_eq!(listing("zwo-camera"), ["libASICamera2.so"]);
        assert_eq!(listing("zwo-focuser"), ["libEAFFocuser.so"]);
        assert_eq!(listing("svbony-camera"), Vec::<String>::new());
        assert_eq!(
            fs::read_to_string(camera_lib.join("libASICamera2.so")).unwrap(),
            "libASICamera2"
        );
    }

    #[test]
    fn stamps_a_nightly_on_the_deb_and_renders_it_for_the_rpm() {
        let fixture = fixture(&["filemonitor"]);
        let nightly = "0.1.0+nightly.202610220507.gabc1234";
        let request = Request {
            services: Some(vec!["filemonitor".to_owned()]),
            rpm: true,
            version: Some(nightly.to_owned()),
            ..Request::default()
        };
        let mut runner = Fake::default();
        let dist = build(&mut runner, fixture.root.path(), &fixture.host, &request).unwrap();
        assert_eq!(dist, fixture.root.path().join("dist").join(nightly));
        let out = format!("<root>/dist/{nightly}/");
        let lines = lines(&runner.lines(), &fixture);
        assert_eq!(
            lines[lines.len() - 2..],
            [
                format!(
                    "cargo deb -p filemonitor --no-build --no-strip --deb-version {nightly} \
                     --output {out}"
                ),
                format!(
                    "cargo generate-rpm -p services/filemonitor -o {out} --set-metadata \
                     version = \"0.1.0^202610220507.gabc1234\""
                ),
            ]
        );
    }

    #[test]
    fn a_release_rpm_takes_its_version_from_cargo() {
        assert_eq!(rpm_version("0.1.0"), None);
        assert_eq!(
            rpm_version("0.1.0+nightly.202610220507.gabc1234").as_deref(),
            Some("0.1.0^202610220507.gabc1234")
        );
    }

    #[test]
    fn refuses_another_version_and_an_unpackaged_service() {
        let fixture = fixture(&["filemonitor"]);
        let request = Request {
            version: Some("0.2.0".to_owned()),
            ..Request::default()
        };
        let mut runner = Fake::default();
        let error = build(&mut runner, fixture.root.path(), &fixture.host, &request).unwrap_err();
        assert!(matches!(error, Error::BadVersion { .. }), "{error}");
        let request = Request {
            services: Some(vec!["doctor".to_owned()]),
            ..Request::default()
        };
        let error = build(&mut runner, fixture.root.path(), &fixture.host, &request).unwrap_err();
        assert_eq!(
            error.to_string(),
            "unknown or unpackaged service: doctor (packaged: filemonitor svbony-camera)"
        );
        assert!(runner.ran.is_empty(), "{:?}", runner.lines());
    }

    #[test]
    fn installs_what_apt_lacks_with_sudo_and_the_cargo_packagers() {
        let fixture = fixture(&["filemonitor"]);
        let request = Request {
            services: Some(vec!["filemonitor".to_owned()]),
            rpm: true,
            ..Request::default()
        };
        let mut runner = Script::default()
            .on("dpkg -s clang", || Err(()))
            .on("dpkg -s libudev-dev", || Err(()))
            .on("id -u", || Ok("1000\n".to_owned()))
            .on("cargo deb --version", || Err(()))
            .on("cargo generate-rpm --version", || Err(()));
        build(&mut runner, fixture.root.path(), &fixture.host, &request).unwrap();
        let installs: Vec<&String> = runner
            .lines
            .iter()
            .filter(|line| line.starts_with("sudo") || line.starts_with("cargo install"))
            .collect();
        assert_eq!(
            installs,
            [
                "sudo apt-get update -qq",
                "sudo apt-get install -y --no-install-recommends libudev-dev clang",
                "cargo install --locked cargo-deb",
                "cargo install --locked cargo-generate-rpm",
            ]
        );
    }

    #[test]
    fn installs_as_root_without_sudo_and_names_the_packages_without_apt() {
        let fixture = fixture(&["filemonitor"]);
        let request = Request {
            services: Some(vec!["filemonitor".to_owned()]),
            ..Request::default()
        };
        let mut runner = Script::default()
            .on("dpkg -s git", || Err(()))
            .on("id -u", || Ok("0\n".to_owned()));
        build(&mut runner, fixture.root.path(), &fixture.host, &request).unwrap();
        assert!(runner
            .lines
            .contains(&"apt-get install -y --no-install-recommends git".to_owned()));
        // cargo-generate-rpm only for --rpm.
        assert!(!runner
            .lines
            .iter()
            .any(|line| line.starts_with("cargo generate-rpm")));

        let host = Host {
            apt: false,
            ..fixture.host.clone()
        };
        let mut runner = Fake::default();
        build(&mut runner, fixture.root.path(), &host, &request).unwrap();
        assert!(
            !runner.lines().iter().any(|line| line.starts_with("dpkg")),
            "{:?}",
            runner.lines()
        );
    }

    #[test]
    fn downloads_each_uncached_sdk_from_its_pin_for_the_arch() {
        let fixture = fixture(&["qhy-camera", "zwo-focuser"]);
        fs::remove_dir_all(fixture.cache_dir.path()).unwrap();
        let host = Host {
            arch: Arch::Aarch64,
            ..fixture.host.clone()
        };
        // The fake curl writes nothing, so each rename after it fails: what
        // matters here is the URL it was given.
        let mut runner = Fake::default();
        build(&mut runner, fixture.root.path(), &host, &Request::default()).unwrap_err();
        let curl = |part: &str, url: &str| format!("curl -fsSL -o <cache>/{part}.part {url}");
        let v = qhy::VERSION;
        let dotless = v.replace('.', "");
        assert_eq!(
            lines(&runner.lines(), &fixture).last().unwrap(),
            &curl(
                &format!("sdk_linux_arm64_{v}.tar.gz"),
                &format!(
                    "https://www.qhyccd.com/file/repository/publish/SDK/{dotless}/sdk_linux_arm64_{v}.tar.gz"
                )
            )
        );

        let request = Request {
            services: Some(vec!["zwo-focuser".to_owned()]),
            ..Request::default()
        };
        let mut runner = Fake::default();
        build(&mut runner, fixture.root.path(), &host, &request).unwrap_err();
        assert_eq!(
            lines(&runner.lines(), &fixture).last().unwrap(),
            &curl(
                &format!("zwo-{ZWO_REF}-armv8/libEAFFocuser.so"),
                &format!(
                    "https://github.com/indilib/indi-3rdparty/raw/{ZWO_REF}/libasi/armv8/libEAFFocuser.bin"
                )
            )
        );

        let request = Request {
            services: Some(vec!["svbony-camera".to_owned()]),
            ..Request::default()
        };
        let mut runner = Fake::default();
        build(&mut runner, fixture.root.path(), &host, &request).unwrap_err();
        assert_eq!(
            lines(&runner.lines(), &fixture).last().unwrap(),
            &curl(
                &format!("svbony-{SVBONY_REF}-aarch64/libSVBCameraSDK.so"),
                &format!(
                    "https://github.com/indilib/indi-3rdparty/raw/{SVBONY_REF}/libsvbony/libSVBCameraSDK_armv8.bin"
                )
            )
        );
    }

    /// indi-3rdparty names its x86-64 blobs one way for ZWO and another
    /// for `SVBony`.
    #[test]
    fn downloads_the_x86_64_blobs_by_their_own_names() {
        let fixture = fixture(&["zwo-camera"]);
        fs::remove_dir_all(fixture.cache_dir.path()).unwrap();
        let mut runner = Fake::default();
        let only = |service: &str| Request {
            services: Some(vec![service.to_owned()]),
            ..Request::default()
        };
        build(
            &mut runner,
            fixture.root.path(),
            &fixture.host,
            &only("zwo-camera"),
        )
        .unwrap_err();
        build(
            &mut runner,
            fixture.root.path(),
            &fixture.host,
            &only("svbony-camera"),
        )
        .unwrap_err();
        let urls: Vec<String> = runner
            .lines()
            .iter()
            .filter_map(|line| line.rsplit(' ').next().map(str::to_owned))
            .filter(|url| url.starts_with("https://"))
            .collect();
        assert_eq!(
            urls,
            [
                format!(
                    "https://github.com/indilib/indi-3rdparty/raw/{ZWO_REF}/libasi/x64/libASICamera2.bin"
                ),
                format!(
                    "https://github.com/indilib/indi-3rdparty/raw/{SVBONY_REF}/libsvbony/libSVBCameraSDK_amd64.bin"
                ),
            ]
        );
    }

    #[test]
    fn refuses_to_download_when_told_to_stay_offline() {
        let fixture = fixture(&["zwo-camera"]);
        fs::remove_file(
            fixture
                .host
                .cache
                .join(format!("zwo-{ZWO_REF}-x64/libASICamera2.so")),
        )
        .unwrap();
        let request = Request {
            skip_sdk_staging: true,
            services: Some(vec!["zwo-camera".to_owned()]),
            ..Request::default()
        };
        let mut runner = Fake::default();
        let error = build(&mut runner, fixture.root.path(), &fixture.host, &request).unwrap_err();
        assert!(
            error.to_string().contains("--skip-sdk-staging set"),
            "{error}"
        );
        assert!(
            !runner.lines().iter().any(|line| line.starts_with("curl")),
            "{:?}",
            runner.lines()
        );
    }

    #[test]
    fn refuses_a_qhy_archive_with_the_wrong_sha256() {
        let fixture = fixture(&["qhy-camera"]);
        fs::remove_dir_all(
            fixture
                .host
                .cache
                .join(format!("qhy-sdk-{}-x86_64", qhy::VERSION)),
        )
        .unwrap();
        fs::write(
            fixture.host.cache.join(qhy::linux_x86_64_archive()),
            "not the archive",
        )
        .unwrap();
        let request = Request {
            services: Some(vec!["qhy-camera".to_owned()]),
            ..Request::default()
        };
        let mut runner = Fake::default();
        let error = build(&mut runner, fixture.root.path(), &fixture.host, &request).unwrap_err();
        assert!(matches!(error, Error::Sha256Mismatch { .. }), "{error}");
        assert!(
            !runner.lines().iter().any(|line| line.starts_with("tar")),
            "{:?}",
            runner.lines()
        );
    }

    #[test]
    fn refuses_another_host() {
        assert_eq!(check_host("linux", "x86_64").unwrap(), Arch::X86_64);
        assert_eq!(check_host("linux", "aarch64").unwrap(), Arch::Aarch64);
        assert!(check_host("macos", "aarch64")
            .unwrap_err()
            .contains("on Linux only"));
        assert!(check_host("linux", "riscv64")
            .unwrap_err()
            .contains("unsupported architecture riscv64"));
    }
}
