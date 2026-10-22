//! `cargo ci build-tarballs`: builds the per-service macOS arm64 tarballs the
//! Homebrew formulas point at (docs/packaging-macos.md), into
//! `dist/<version>/` with a `SHA256SUMS.txt`.
//!
//! 1. It stages the native SDKs in the cache ([`packaging::cache_dir`]):
//!    QHYCCD's static `libqhyccd.a`, which qhy-camera links, and the ZWO
//!    dylibs each zwo service links and ships, ASI's for zwo-camera and EAF's
//!    for zwo-focuser (ADR-014). The ZWO dylibs come from the commit
//!    `install-zwo-sdk` pins, with `@rpath/` install names already; the one
//!    change is the ASI dylib's libusb, which it loads by `@rpath` and which
//!    nothing on a Mac provides there, pointed at Homebrew's, which the
//!    formula depends on.
//! 2. It release-builds the services with two `@loader_path` rpaths, `../lib`
//!    for the Homebrew keg and `lib` for an unpacked tarball, uniform across
//!    the binaries. Each zwo service builds in a cargo run of its own: cargo
//!    unifies features within a run, so building them together would link
//!    both binaries to every ZWO SDK.
//! 3. It packs each service as `rusty-photon-<svc>`, with sentinel's doctor,
//!    and each zwo service's dylib under `lib/` with the MIT license.
//!
//! svbony-camera has no macOS SDK, so no tarball ([`packaging::NO_MACOS_SDK`]).

use std::fs;
use std::path::{Path, PathBuf};

use crate::packaging::{self, io, qhy, Error};
use crate::process::{Cmd, Runner};

/// Homebrew's libusb on Apple Silicon.
///
/// qhy-camera links it and the ASI dylib
/// loads it, so this path ships inside that dylib: it is the target Mac's,
/// not the build host's, and `/opt/homebrew` is the one Homebrew prefix on
/// Apple Silicon. A host without it fails here rather than baking another
/// path into the artifact.
pub const LIBUSB: &str = "/opt/homebrew/opt/libusb";

/// Where the Xcode command line tools keep libclang, which bindgen finds
/// without `LIBCLANG_PATH`.
pub const CLT_LIBCLANG: &str = "/Library/Developer/CommandLineTools/usr/lib/libclang.dylib";

/// The two rpaths every binary gets. It replaces any `RUSTFLAGS` the caller
/// has, so the binaries do not depend on the shell they were built from.
const RUSTFLAGS: &str =
    "-C link-arg=-Wl,-rpath,@loader_path/../lib -C link-arg=-Wl,-rpath,@loader_path/lib";

/// What a build makes.
#[derive(Clone, Debug, Default)]
pub struct Request {
    /// Only these services. `None` builds every packaged one with a macOS SDK.
    pub services: Option<Vec<String>>,
    /// Download nothing: rebuild from the SDKs a previous run cached.
    pub skip_sdk_staging: bool,
    /// The version to stamp: the workspace's when `None`, or a nightly stamp
    /// on it.
    pub version: Option<String>,
}

/// The host's paths that a build reads, so the tests can stand in for a Mac.
#[derive(Clone, Debug)]
pub struct Host {
    /// [`LIBUSB`].
    pub libusb: PathBuf,
    /// [`CLT_LIBCLANG`].
    pub clt_libclang: PathBuf,
    /// The caller's `LIBCLANG_PATH`, if any.
    pub libclang_path: Option<String>,
    /// [`packaging::cache_dir`].
    pub cache: PathBuf,
}

impl Host {
    /// This machine's.
    #[must_use]
    pub fn this() -> Self {
        Self {
            libusb: PathBuf::from(LIBUSB),
            clt_libclang: PathBuf::from(CLT_LIBCLANG),
            libclang_path: std::env::var("LIBCLANG_PATH")
                .ok()
                .filter(|path| !path.is_empty()),
            cache: packaging::cache_dir(),
        }
    }
}

/// Fails unless this is an Apple Silicon Mac, the one macOS target, where
/// the tarballs are built and verified.
///
/// # Errors
///
/// On another OS or architecture.
pub fn check_host(os: &str, arch: &str) -> Result<(), String> {
    if os != "macos" {
        return Err(
            "the macOS packages are built and verified on macOS only (Linux ships .deb/.rpm — \
             scripts/build-packages.sh)"
                .to_owned(),
        );
    }
    if arch != "aarch64" {
        return Err("Intel macOS is not a target: the macOS packages are arm64 only".to_owned());
    }
    Ok(())
}

/// The environment the release builds need, beyond [`RUSTFLAGS`].
#[derive(Debug, Default)]
struct Staged {
    /// `QHYCCD_SDK_DIR`: where `libqhyccd.a` is.
    qhy: Option<PathBuf>,
    /// `ZWO_SDK_LIB_DIR`: the ZWO dylibs, also what the tarballs ship.
    zwo: Option<PathBuf>,
    /// `LIBCLANG_PATH`, when bindgen needs Homebrew's.
    libclang: Option<String>,
}

/// Stages QHYCCD's SDK and returns the directory holding `libqhyccd.a`.
fn stage_qhy(runner: &mut impl Runner, cache: &Path, offline: bool) -> Result<PathBuf, Error> {
    let file = format!("sdkmac_dir_arm_{}.tar.gz", qhy::VERSION);
    let extract = cache.join(format!("qhy-sdk-{}-mac-arm64", qhy::VERSION));
    if !extract.is_dir() {
        if offline {
            return Err(missing(&extract));
        }
        let archive = cache.join(&file);
        if !archive.is_file() {
            packaging::download(runner, &qhy::url(&file), &archive)?;
        }
        packaging::check_sha256(&archive, qhy::SHA256_MAC_ARM64)?;
        let work = tempfile::tempdir_in(cache).map_err(io(cache))?;
        let work_arg = work.path().to_string_lossy();
        let archive_arg = archive.to_string_lossy();
        runner.run(&Cmd::new("tar", &["-xzf", &archive_arg, "-C", &work_arg]))?;
        let unpacked = work.path().join(file.trim_end_matches(".tar.gz"));
        fs::rename(&unpacked, &extract).map_err(io(&unpacked))?;
    }
    // Found rather than assumed: the archive's layout is QHYCCD's.
    let lib = find(&extract, "libqhyccd.a")?.ok_or_else(|| Error::Io {
        path: extract.display().to_string(),
        source: std::io::Error::new(std::io::ErrorKind::NotFound, "no libqhyccd.a under it"),
    })?;
    let dir = lib.parent().map(Path::to_path_buf).unwrap_or_default();
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

fn missing(path: &Path) -> Error {
    Error::Io {
        path: path.display().to_string(),
        source: std::io::Error::new(
            std::io::ErrorKind::NotFound,
            "--skip-sdk-staging set, but a previous run has not cached it",
        ),
    }
}

/// The ZWO dylibs the selected services link.
fn zwo_dylibs(services: &[String]) -> Vec<&'static str> {
    let mut dylibs = Vec::new();
    if services.iter().any(|service| service == "zwo-camera") {
        dylibs.push("libASICamera2");
    }
    if services.iter().any(|service| service == "zwo-focuser") {
        dylibs.push("libEAFFocuser");
    }
    dylibs
}

/// Stages the ZWO dylibs and returns their directory, and the
/// `LIBCLANG_PATH` bindgen needs when the host has no libclang of its own.
fn stage_zwo(
    runner: &mut impl Runner,
    root: &Path,
    host: &Host,
    dylibs: &[&str],
    offline: bool,
) -> Result<(PathBuf, Option<String>), Error> {
    // libzwo-sys's build script runs bindgen, which needs libclang: the
    // Xcode command line tools', which it finds itself, or Homebrew's llvm.
    let libclang = if host.libclang_path.is_none() && !host.clt_libclang.exists() {
        let llvm = runner.read(&Cmd::new("brew", &["--prefix", "llvm"]))?;
        let lib = Path::new(llvm.trim_end()).join("lib");
        if !lib.is_dir() {
            println!("Installing llvm via Homebrew (bindgen needs libclang)");
            runner.run(&Cmd::new("brew", &["install", "llvm"]))?;
        }
        Some(lib.to_string_lossy().into_owned())
    } else {
        None
    };

    let commit = packaging::sdk_ref(root, packaging::ZWO_MODULE)?;
    let dir = host.cache.join(format!("zwo-{commit}-mac_arm64"));
    fs::create_dir_all(&dir).map_err(io(&dir))?;
    for dylib in dylibs {
        let path = dir.join(format!("{dylib}.dylib"));
        if !path.is_file() {
            if offline {
                return Err(missing(&path));
            }
            let url = format!(
                "https://github.com/indilib/indi-3rdparty/raw/{commit}/libasi/mac_arm64/{dylib}.bin"
            );
            packaging::download(runner, &url, &path)?;
        }
    }
    // A no-op once the cached copy is rewritten. EAF's dylib loads only
    // system frameworks.
    if dylibs.contains(&"libASICamera2") {
        let libusb = host.libusb.join("lib/libusb-1.0.0.dylib");
        let asi = dir.join("libASICamera2.dylib");
        runner.run(&Cmd::new(
            "install_name_tool",
            &[
                "-change",
                "@rpath/libusb-1.0.0.dylib",
                &libusb.to_string_lossy(),
                &asi.to_string_lossy(),
            ],
        ))?;
    }
    println!(
        "ZWO SDK dylibs (indi-3rdparty {commit}) staged: {}; ZWO_SDK_LIB_DIR={}",
        dylibs.join(" "),
        dir.display()
    );
    Ok((dir, libclang))
}

/// The cargo runs that build `services`: the zwo services each alone, the
/// rest together, with doctor when sentinel is among them, since its tarball
/// carries doctor.
fn cargo_runs(services: &[String]) -> Vec<Vec<String>> {
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

fn cargo_build(staged: &Staged, root: &Path, packages: &[String]) -> Cmd {
    let mut args = vec!["build", "--release"];
    args.extend(packages.iter().map(String::as_str));
    let mut cmd = Cmd::new("cargo", &args)
        .dir(root)
        .env("RUSTFLAGS", RUSTFLAGS);
    if let Some(dir) = &staged.qhy {
        cmd = cmd.env("QHYCCD_SDK_DIR", &dir.to_string_lossy());
    }
    if let Some(dir) = &staged.zwo {
        cmd = cmd.env("ZWO_SDK_LIB_DIR", &dir.to_string_lossy());
    }
    if let Some(path) = &staged.libclang {
        cmd = cmd.env("LIBCLANG_PATH", path);
    }
    cmd
}

/// The tarball's name for `service` at `version`.
#[must_use]
pub fn tarball(service: &str, version: &str) -> String {
    format!("rusty-photon-{service}-{version}-aarch64-apple-darwin.tar.gz")
}

/// Packs `service`'s tarball into `dist`.
fn pack(
    runner: &mut impl Runner,
    root: &Path,
    staged: &Staged,
    service: &str,
    dist: &Path,
    version: &str,
) -> Result<(), Error> {
    println!("Packaging {service}");
    let release = root.join("target/release");
    let stage = tempfile::tempdir().map_err(io(&std::env::temp_dir()))?;
    let copy = |from: &Path, to: &Path| fs::copy(from, to).map(|_| ()).map_err(io(from));
    // Cargo's binary names stay; the tarball ships them renamed, as the deb
    // does (ADR-012).
    let mut contents = vec![format!("rusty-photon-{service}")];
    copy(
        &release.join(service),
        &stage.path().join(format!("rusty-photon-{service}")),
    )?;
    if service == "sentinel" {
        // The formula writes the renewal plist itself at install time.
        copy(
            &release.join("doctor"),
            &stage.path().join("rusty-photon-doctor"),
        )?;
        contents.push("rusty-photon-doctor".to_owned());
    }
    if let Some(dylib) = zwo_dylibs(&[service.to_owned()]).first() {
        let zwo = staged.zwo.as_deref().unwrap_or_else(|| Path::new(""));
        let lib = stage.path().join("lib");
        fs::create_dir(&lib).map_err(io(&lib))?;
        copy(
            &zwo.join(format!("{dylib}.dylib")),
            &lib.join(format!("{dylib}.dylib")),
        )?;
        copy(
            &root.join(format!("services/{service}/pkg/ZWO-SDK-LICENSE.txt")),
            &stage.path().join("ZWO-SDK-LICENSE.txt"),
        )?;
        contents.extend(["lib".to_owned(), "ZWO-SDK-LICENSE.txt".to_owned()]);
    }
    let out = dist.join(tarball(service, version));
    let out = out.to_string_lossy();
    let stage_arg = stage.path().to_string_lossy();
    let mut args = vec!["-czf", out.as_ref(), "-C", stage_arg.as_ref()];
    args.extend(contents.iter().map(String::as_str));
    // No AppleDouble `._*` entries in the tarballs.
    runner.run(&Cmd::new("tar", &args).env("COPYFILE_DISABLE", "1"))?;
    Ok(())
}

/// Builds the tarballs `request` asks for and returns the directory they are
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
    let services = packaging::macos_services(&packaged, request.services.as_deref())?;
    if request.services.is_none() && packaged.iter().any(|svc| svc == packaging::NO_MACOS_SDK) {
        println!(
            "Skipping {}: no confirmed mac_arm64 SVBony SDK blob (indi-3rdparty ships mac64/Intel only)",
            packaging::NO_MACOS_SDK
        );
    }
    let needs_qhy = services.iter().any(|service| service == "qhy-camera");
    let dylibs = zwo_dylibs(&services);

    if (needs_qhy || dylibs.contains(&"libASICamera2"))
        && !host.libusb.join("lib/libusb-1.0.0.dylib").exists()
    {
        println!("Installing libusb via Homebrew");
        runner.run(&Cmd::new("brew", &["install", "libusb"]))?;
        if !host.libusb.join("lib/libusb-1.0.0.dylib").exists() {
            return Err(Error::Io {
                path: host.libusb.display().to_string(),
                source: std::io::Error::new(
                    std::io::ErrorKind::NotFound,
                    "libusb did not land here — a default-prefix (/opt/homebrew) Homebrew is \
                     required, as the path ships inside the zwo dylib",
                ),
            });
        }
    }

    fs::create_dir_all(&host.cache).map_err(io(&host.cache))?;
    let mut staged = Staged::default();
    if needs_qhy {
        staged.qhy = Some(stage_qhy(runner, &host.cache, request.skip_sdk_staging)?);
    }
    if !dylibs.is_empty() {
        let (dir, libclang) = stage_zwo(runner, root, host, &dylibs, request.skip_sdk_staging)?;
        staged.zwo = Some(dir);
        staged.libclang = libclang;
    }

    for packages in cargo_runs(&services) {
        println!("Building release binaries: {}", packages.join(" "));
        runner.run(&cargo_build(&staged, root, &packages))?;
    }
    let mut binaries = services.clone();
    if services.iter().any(|service| service == "sentinel") {
        binaries.push("doctor".to_owned());
    }
    for binary in &binaries {
        let path = root.join("target/release").join(binary);
        runner.run(&Cmd::new("strip", &[&path.to_string_lossy()]))?;
    }

    let dist = root.join("dist").join(&version);
    fs::create_dir_all(&dist).map_err(io(&dist))?;
    for service in &services {
        pack(runner, root, &staged, service, &dist, &version)?;
    }
    packaging::write_sums(&dist, ".tar.gz")?;
    Ok(dist)
}

// The tools run on a Mac; their tests compare Unix paths.
#[cfg(test)]
#[cfg(unix)]
mod tests {
    use super::*;
    use crate::process::Fake;

    const REF: &str = "b0802f28055b67aa6a99580d260c3bb4c27eba4b";

    /// A repository and a Mac: `services` packaged, svbony-camera too, each
    /// with a release binary, the ZWO module's pin, a cached SDK of each kind,
    /// Homebrew's libusb and the Xcode libclang.
    struct Fixture {
        root: tempfile::TempDir,
        host: Host,
        mac_dir: tempfile::TempDir,
    }

    fn fixture(services: &[&str]) -> Fixture {
        let root = tempfile::tempdir().unwrap();
        let r = root.path();
        fs::write(
            r.join("Cargo.toml"),
            "[workspace]\nmembers = []\n\n[workspace.package]\nversion = \"0.1.0\"\n",
        )
        .unwrap();
        fs::create_dir_all(r.join("target/release")).unwrap();
        for service in services.iter().chain(&["svbony-camera"]) {
            fs::create_dir_all(r.join("services").join(service).join("pkg")).unwrap();
            fs::write(r.join("target/release").join(service), service).unwrap();
        }
        fs::write(r.join("target/release/doctor"), "doctor").unwrap();
        for service in ["zwo-camera", "zwo-focuser"] {
            if services.contains(&service) {
                let license = r
                    .join("services")
                    .join(service)
                    .join("pkg/ZWO-SDK-LICENSE.txt");
                fs::write(license, "MIT").unwrap();
            }
        }
        let module = r.join(packaging::ZWO_MODULE);
        fs::create_dir_all(module.parent().unwrap()).unwrap();
        fs::write(&module, format!("export const REF = '{REF}';\n")).unwrap();

        let mac = tempfile::tempdir().unwrap();
        let m = mac.path();
        let cache = m.join("cache");
        let qhy = cache.join(format!(
            "qhy-sdk-{v}-mac-arm64/sdkmac_dir_arm_{v}/lib",
            v = qhy::VERSION
        ));
        fs::create_dir_all(&qhy).unwrap();
        fs::write(qhy.join("libqhyccd.a"), "a").unwrap();
        let zwo = cache.join(format!("zwo-{REF}-mac_arm64"));
        fs::create_dir_all(&zwo).unwrap();
        for dylib in ["libASICamera2", "libEAFFocuser"] {
            fs::write(zwo.join(format!("{dylib}.dylib")), dylib).unwrap();
        }
        fs::create_dir_all(m.join("libusb/lib")).unwrap();
        fs::write(m.join("libusb/lib/libusb-1.0.0.dylib"), "usb").unwrap();
        fs::write(m.join("libclang.dylib"), "clang").unwrap();
        let host = Host {
            libusb: m.join("libusb"),
            clt_libclang: m.join("libclang.dylib"),
            libclang_path: None,
            cache,
        };
        Fixture {
            root,
            host,
            mac_dir: mac,
        }
    }

    /// The commands, with the fixture's paths shortened to `<root>` and
    /// `<mac>`, and the tar stage directories to `<stage>`.
    fn lines(runner: &Fake, fixture: &Fixture) -> Vec<String> {
        let root = fixture.root.path().display().to_string();
        let mac = fixture.mac_dir.path().display().to_string();
        runner
            .ran
            .iter()
            .map(|cmd| {
                let mut cmd = cmd.clone();
                if cmd.program == "tar" && cmd.args.first().is_some_and(|arg| arg == "-czf") {
                    cmd.args[3] = "<stage>".to_owned();
                }
                cmd.to_string()
                    .replace(&root, "<root>")
                    .replace(&mac, "<mac>")
            })
            .collect()
    }

    const ENV: &str = "RUSTFLAGS=-C link-arg=-Wl,-rpath,@loader_path/../lib -C link-arg=-Wl,-rpath,@loader_path/lib";

    #[test]
    fn builds_and_packs_every_packaged_service_but_svbony_camera() {
        let fixture = fixture(&[
            "filemonitor",
            "qhy-camera",
            "sentinel",
            "zwo-camera",
            "zwo-focuser",
        ]);
        let mut runner = Fake::default();
        let dist = build(
            &mut runner,
            fixture.root.path(),
            &fixture.host,
            &Request::default(),
        )
        .unwrap();
        assert_eq!(dist, fixture.root.path().join("dist/0.1.0"));
        let qhy = format!(
            "QHYCCD_SDK_DIR=<mac>/cache/qhy-sdk-{v}-mac-arm64/sdkmac_dir_arm_{v}/lib",
            v = qhy::VERSION
        );
        let zwo = format!("ZWO_SDK_LIB_DIR=<mac>/cache/zwo-{REF}-mac_arm64");
        let tar = |svc: &str, contents: &str| {
            format!(
                "COPYFILE_DISABLE=1 tar -czf <root>/dist/0.1.0/rusty-photon-{svc}-0.1.0-aarch64-apple-darwin.tar.gz -C <stage> {contents}"
            )
        };
        assert_eq!(
            lines(&runner, &fixture),
            [
                format!(
                    "install_name_tool -change @rpath/libusb-1.0.0.dylib <mac>/libusb/lib/libusb-1.0.0.dylib <mac>/cache/zwo-{REF}-mac_arm64/libASICamera2.dylib"
                ),
                format!(
                    "{ENV} {qhy} {zwo} cargo build --release -p filemonitor -p qhy-camera \
                     -p sentinel -p doctor"
                ),
                format!("{ENV} {qhy} {zwo} cargo build --release -p zwo-camera"),
                format!("{ENV} {qhy} {zwo} cargo build --release -p zwo-focuser"),
                "strip <root>/target/release/filemonitor".to_owned(),
                "strip <root>/target/release/qhy-camera".to_owned(),
                "strip <root>/target/release/sentinel".to_owned(),
                "strip <root>/target/release/zwo-camera".to_owned(),
                "strip <root>/target/release/zwo-focuser".to_owned(),
                "strip <root>/target/release/doctor".to_owned(),
                tar("filemonitor", "rusty-photon-filemonitor"),
                tar("qhy-camera", "rusty-photon-qhy-camera"),
                tar("sentinel", "rusty-photon-sentinel rusty-photon-doctor"),
                tar("zwo-camera", "rusty-photon-zwo-camera lib ZWO-SDK-LICENSE.txt"),
                tar("zwo-focuser", "rusty-photon-zwo-focuser lib ZWO-SDK-LICENSE.txt"),
            ]
        );
        assert!(dist.join("SHA256SUMS.txt").is_file());
    }

    #[test]
    fn stages_nothing_a_service_list_does_not_need() {
        let fixture = fixture(&["filemonitor", "qhy-camera"]);
        let request = Request {
            services: Some(vec!["filemonitor".to_owned()]),
            ..Request::default()
        };
        let mut runner = Fake::default();
        build(&mut runner, fixture.root.path(), &fixture.host, &request).unwrap();
        let lines = lines(&runner, &fixture);
        assert_eq!(
            lines[0],
            format!("{ENV} cargo build --release -p filemonitor")
        );
        assert_eq!(lines.len(), 3, "{lines:?}");
    }

    #[test]
    fn stamps_a_nightly_version_and_refuses_another() {
        let fixture = fixture(&["filemonitor"]);
        let nightly = "0.1.0+nightly.202610220507.gabc1234";
        let request = Request {
            version: Some(nightly.to_owned()),
            ..Request::default()
        };
        let dist = build(
            &mut Fake::default(),
            fixture.root.path(),
            &fixture.host,
            &request,
        )
        .unwrap();
        assert_eq!(dist, fixture.root.path().join("dist").join(nightly));
        let request = Request {
            version: Some("0.2.0".to_owned()),
            ..Request::default()
        };
        let error = build(
            &mut Fake::default(),
            fixture.root.path(),
            &fixture.host,
            &request,
        )
        .unwrap_err();
        assert!(matches!(error, Error::BadVersion { .. }), "{error}");
    }

    #[test]
    fn refuses_svbony_camera() {
        let fixture = fixture(&["filemonitor"]);
        let request = Request {
            services: Some(vec!["svbony-camera".to_owned()]),
            ..Request::default()
        };
        let error = build(
            &mut Fake::default(),
            fixture.root.path(),
            &fixture.host,
            &request,
        )
        .unwrap_err();
        assert!(matches!(error, Error::NoMacosSdk), "{error}");
    }

    #[test]
    fn downloads_an_uncached_zwo_dylib_from_the_pinned_commit() {
        let fixture = fixture(&["zwo-focuser"]);
        let cached = fixture
            .host
            .cache
            .join(format!("zwo-{REF}-mac_arm64/libEAFFocuser.dylib"));
        fs::remove_file(&cached).unwrap();
        // The fake curl writes nothing, so the rename after it fails: what
        // matters here is the URL it was given.
        let mut runner = Fake::default();
        build(
            &mut runner,
            fixture.root.path(),
            &fixture.host,
            &Request::default(),
        )
        .unwrap_err();
        let lines = lines(&runner, &fixture);
        assert_eq!(
            lines,
            [format!(
                "curl -fsSL -o <mac>/cache/zwo-{REF}-mac_arm64/libEAFFocuser.dylib.part \
                 https://github.com/indilib/indi-3rdparty/raw/{REF}/libasi/mac_arm64/libEAFFocuser.bin"
            )]
        );
    }

    #[test]
    fn refuses_to_download_when_told_to_stay_offline() {
        let fixture = fixture(&["qhy-camera"]);
        fs::remove_dir_all(
            fixture
                .host
                .cache
                .join(format!("qhy-sdk-{}-mac-arm64", qhy::VERSION)),
        )
        .unwrap();
        let request = Request {
            skip_sdk_staging: true,
            ..Request::default()
        };
        let mut runner = Fake::default();
        let error = build(&mut runner, fixture.root.path(), &fixture.host, &request).unwrap_err();
        assert!(
            error.to_string().contains("--skip-sdk-staging set"),
            "{error}"
        );
        assert!(runner.ran.is_empty(), "{:?}", runner.lines());
    }

    #[test]
    fn refuses_a_qhy_archive_with_the_wrong_sha256() {
        let fixture = fixture(&["qhy-camera"]);
        fs::remove_dir_all(
            fixture
                .host
                .cache
                .join(format!("qhy-sdk-{}-mac-arm64", qhy::VERSION)),
        )
        .unwrap();
        let archive = format!("sdkmac_dir_arm_{}.tar.gz", qhy::VERSION);
        fs::write(fixture.host.cache.join(archive), "not the archive").unwrap();
        let mut runner = Fake::default();
        let error = build(
            &mut runner,
            fixture.root.path(),
            &fixture.host,
            &Request::default(),
        )
        .unwrap_err();
        assert!(matches!(error, Error::Sha256Mismatch { .. }), "{error}");
        assert!(runner.ran.is_empty(), "{:?}", runner.lines());
    }

    #[test]
    fn installs_libusb_through_homebrew_and_fails_when_it_lands_elsewhere() {
        let fixture = fixture(&["qhy-camera"]);
        fs::remove_file(fixture.host.libusb.join("lib/libusb-1.0.0.dylib")).unwrap();
        let mut runner = Fake::default();
        let error = build(
            &mut runner,
            fixture.root.path(),
            &fixture.host,
            &Request::default(),
        )
        .unwrap_err();
        assert_eq!(runner.lines(), ["brew install libusb"]);
        assert!(
            error
                .to_string()
                .contains("default-prefix (/opt/homebrew) Homebrew"),
            "{error}"
        );
    }

    #[test]
    fn takes_homebrews_libclang_when_the_host_has_none() {
        let fixture = fixture(&["zwo-focuser"]);
        let host = Host {
            clt_libclang: fixture.host.cache.join("absent"),
            ..fixture.host.clone()
        };
        let llvm = fixture.mac_dir.path().join("llvm");
        fs::create_dir_all(llvm.join("lib")).unwrap();
        let answer: &'static str = Box::leak(format!("{}\n", llvm.display()).into_boxed_str());
        let mut runner = Fake::answering(&[answer]);
        build(&mut runner, fixture.root.path(), &host, &Request::default()).unwrap();
        let lines = lines(&runner, &fixture);
        assert_eq!(lines[0], "brew --prefix llvm");
        assert!(
            lines[1].contains("LIBCLANG_PATH=<mac>/llvm/lib "),
            "{lines:?}"
        );
    }

    #[test]
    fn packs_each_binary_under_its_package_name() {
        let fixture = fixture(&["sentinel"]);
        let mut runner = Fake::default();
        build(
            &mut runner,
            fixture.root.path(),
            &fixture.host,
            &Request::default(),
        )
        .unwrap();
        // The stage is gone with the run; tar was handed its contents.
        let tar = runner.ran.iter().find(|cmd| cmd.program == "tar").unwrap();
        assert_eq!(
            tar.args[4..],
            ["rusty-photon-sentinel", "rusty-photon-doctor"]
        );
    }

    #[test]
    fn refuses_another_host() {
        check_host("macos", "aarch64").unwrap();
        assert!(check_host("linux", "aarch64")
            .unwrap_err()
            .contains("on macOS only"));
        assert!(check_host("macos", "x86_64")
            .unwrap_err()
            .contains("arm64 only"));
    }
}
