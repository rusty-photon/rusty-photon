//! `cargo ci build-msi`: builds the Windows suite MSI, every service in one
//! installer (ADR-015), into `dist/<version>/rusty-photon-<version>-x64.msi`,
//! with a `SHA256SUMS.txt` (docs/packaging-windows.md).
//!
//! It builds on the machine the MSI is for, `x86_64` Windows with the MSVC
//! toolchain and the .NET SDK: windows-latest in CI, or a developer's box.
//!
//! 1. It stages the native SDKs in the cache ([`packaging::cache_dir`]):
//!    QHYCCD's import library, which qhy-camera's delay-load link takes (the
//!    DLL is the operator's, ADR-013), and ZWO's import libraries and DLLs,
//!    ASI's for zwo-camera and EAF's for zwo-focuser (ADR-014), which the MSI
//!    bundles. ZWO publishes its SDKs for Windows behind rolling links only,
//!    with no version to pin or hash to check: the builder reads them from
//!    `install-zwo-sdk`'s module, so it fetches what CI links, and the cache
//!    keeps one download of them, which deleting `zwo-win64` in it refreshes.
//! 2. It release-builds every service the MSI installs, and doctor, which
//!    sentinel's feature carries, CRT-static, so no VC++ redistributable is
//!    needed (ADR-015), each zwo service in a cargo run of its own
//!    ([`packaging::cargo_runs`]).
//! 3. It runs `WiX` at [`WIX_VERSION`] over `installer/`, after installing
//!    that version as a .NET tool when the `wix` on `PATH` is another.
//!
//! A nightly's version, `<base>+nightly.<yyyymmddhhmm>.g<sha>`, names the MSI
//! and its directory and goes in its Programs and Features comments. Its
//! `ProductVersion` is `<base>.<YYDDD>`, the stamp's year and day of the year:
//! Windows Installer compares the first three fields only, so an upgrade sees
//! `<base>` (docs/packaging-windows.md, Nightly channel).

use std::ffi::OsString;
use std::fs;
use std::path::{Path, PathBuf};

use crate::packaging::{self, io, qhy};
use crate::process::{self, Cmd, Runner};

/// The `WiX` release that builds the MSI, and its extensions' (ADR-015). The
/// Windows pool template installs it too
/// (`tools/ci/runner-guest/provision-msi.ps1`).
pub const WIX_VERSION: &str = "5.0.2";

const WIX_EXTENSIONS: &[&str] = &[
    "WixToolset.Util.wixext",
    "WixToolset.Firewall.wixext",
    "WixToolset.UI.wixext",
];

/// The ZWO SDK files the MSI's build stages, by ZWO's names, and the names
/// they take. libzwo-sys links `EAFFocuser`, so EAF's import library takes
/// that name; each DLL keeps ZWO's, which its import library asks the loader
/// for.
const ZWO_FILES: &[(&str, &str)] = &[
    ("ASICamera2.lib", "ASICamera2.lib"),
    ("ASICamera2.dll", "ASICamera2.dll"),
    ("EAF_focuser.lib", "EAFFocuser.lib"),
    ("EAF_focuser.dll", "EAF_focuser.dll"),
];

/// The switches that build a service without its SDK, which no shipped
/// binary may be: the build takes them out of its environment.
const SKIP_NATIVE_LINK: &[&str] = &["QHYCCD_SKIP_NATIVE_LINK", "ZWO_SKIP_NATIVE_LINK"];

/// What a build makes.
#[derive(Clone, Debug, Default)]
pub struct Request {
    /// Download nothing: rebuild from the SDKs a previous run cached.
    pub skip_sdk_staging: bool,
    /// Take the binaries a previous run built and run `WiX` alone, the
    /// installer's authoring loop.
    pub skip_build: bool,
    /// The version to stamp: the workspace's when `None`, or a nightly stamp
    /// on it.
    pub version: Option<String>,
}

/// What a build needs to know of the machine it runs on, so the tests can
/// stand in for one.
#[derive(Clone, Debug)]
pub struct Host {
    /// [`packaging::cache_dir`].
    pub cache: PathBuf,
    /// Whether `dotnet` is on `PATH`: the `WiX` CLI is a .NET tool.
    pub dotnet: bool,
    /// Where `dotnet tool install --global` puts `wix`:
    /// `%USERPROFILE%\.dotnet\tools`.
    pub dotnet_tools: PathBuf,
}

impl Host {
    /// This machine.
    #[must_use]
    pub fn this() -> Self {
        let dotnet = std::env::var_os("PATH").is_some_and(|path| {
            std::env::split_paths(&path).any(|dir| dir.join("dotnet.exe").is_file())
        });
        Self {
            cache: packaging::cache_dir(),
            dotnet,
            dotnet_tools: std::env::var_os("USERPROFILE")
                .map_or_default(PathBuf::from)
                .join(".dotnet")
                .join("tools"),
        }
    }
}

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error(transparent)]
    Packaging(#[from] packaging::Error),
    #[error(transparent)]
    Process(#[from] process::Error),
    #[error("{0}")]
    Refused(String),
}

fn refused(reason: impl Into<String>) -> Error {
    Error::Refused(reason.into())
}

/// Refuses any machine but the one the MSI is built and verified on.
///
/// # Errors
///
/// On another OS or architecture.
pub fn check_host(os: &str, arch: &str) -> Result<(), String> {
    if os == "windows" && arch == "x86_64" {
        Ok(())
    } else {
        Err(format!(
            "the MSI is built and verified on x86_64 Windows only (ADR-015), not {arch} {os} \
             (Linux packages: `cargo ci build-packages`; macOS tarballs: \
             `cargo ci build-tarballs`)"
        ))
    }
}

/// Whether `version` is a plain `x.y.z`.
fn is_plain(version: &str) -> bool {
    let fields: Vec<&str> = version.split('.').collect();
    fields.len() == 3
        && fields
            .iter()
            .all(|field| !field.is_empty() && field.bytes().all(|byte| byte.is_ascii_digit()))
}

/// The `yyyymmddhhmm` of a nightly stamp on `workspace`,
/// `<workspace>+nightly.<yyyymmddhhmm>.g<sha>`.
fn nightly_stamp<'a>(workspace: &str, version: &'a str) -> Option<&'a str> {
    let rest = version.strip_prefix(workspace)?.strip_prefix("+nightly.")?;
    let (stamp, sha) = rest.split_once(".g")?;
    let digits = stamp.len() == 12 && stamp.bytes().all(|byte| byte.is_ascii_digit());
    let hex = (7..=40).contains(&sha.len())
        && sha
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte));
    (digits && hex).then_some(stamp)
}

/// The days of each month of a year that is not a leap year.
const MONTH_DAYS: [u32; 12] = [31, 28, 31, 30, 31, 30, 31, 31, 30, 31, 30, 31];

/// `YYDDD` for the date-time `stamp`, `yyyymmddhhmm`: the year's last two
/// digits, then the day of the year in three. `None` for a date-time that
/// does not exist, such as month 13 or hour 25.
fn yyddd(stamp: &str) -> Option<u32> {
    let field = |from: usize, to: usize| stamp.get(from..to)?.parse::<u32>().ok();
    let (year, month, day) = (field(0, 4)?, field(4, 6)?, field(6, 8)?);
    let (hour, minute) = (field(8, 10)?, field(10, 12)?);
    let leap = year.is_multiple_of(4) && (!year.is_multiple_of(100) || year.is_multiple_of(400));
    let days = |month: u32| {
        let days = MONTH_DAYS.get(usize::try_from(month.checked_sub(1)?).ok()?)?;
        Some(if leap && month == 2 { 29 } else { *days })
    };
    if !(1..=days(month)?).contains(&day) || hour > 23 || minute > 59 {
        return None;
    }
    let before: u32 = (1..month).map(|month| days(month).unwrap_or(0)).sum();
    year.checked_rem(100)?
        .checked_mul(1000)?
        .checked_add(before)?
        .checked_add(day)
}

/// The MSI's `ProductVersion` and the full version that names it, for
/// `version`, which [`packaging::stamp`] has taken: the workspace's for
/// both, or for a nightly stamp `<base>.<YYDDD>` and the stamp.
///
/// # Errors
///
/// When the workspace's version is not a plain `x.y.z`, or the nightly stamp
/// is not `<base>+nightly.<yyyymmddhhmm>.g<sha>` with a real date-time, or
/// its date does not fit a field of a `ProductVersion`.
fn versions(workspace: &str, version: &str) -> Result<(String, String), Error> {
    if !is_plain(workspace) {
        return Err(refused(format!(
            "workspace version '{workspace}' is not a plain x.y.z (the MSI's ProductVersion base)"
        )));
    }
    if version == workspace {
        return Ok((workspace.to_owned(), workspace.to_owned()));
    }
    let stamp = nightly_stamp(workspace, version).ok_or_else(|| {
        refused(format!(
            "--version '{version}' is not {workspace}+nightly.<yyyymmddhhmm>.g<sha>"
        ))
    })?;
    let yyddd = yyddd(stamp).ok_or_else(|| {
        refused(format!(
            "--version stamp '{stamp}' is not a real UTC date-time (yyyymmddhhmm)"
        ))
    })?;
    // A `ProductVersion` field holds at most 65535, which YYDDD passes in
    // 2066.
    if yyddd > 65535 {
        return Err(refused(format!(
            "nightly date field {yyddd} exceeds the MSI's 65535 per-field cap"
        )));
    }
    Ok((format!("{workspace}.{yyddd}"), version.to_owned()))
}

/// The x64 build of the file named `name`, in any case, under `root`: never
/// a 32-bit one (`Win32`, `x86`), nor a static library's or a demo's, which
/// bundles `OpenCV`'s; an `x64` or `amd64` folder first, and among those the
/// first by path, so `lib/Windows/x64` comes before `lib/Windows7/x64`. The
/// rule `install-zwo-sdk`'s `pickX64` follows.
///
/// # Errors
///
/// When a directory under `root` cannot be read.
fn pick_x64(root: &Path, name: &str) -> Result<Option<PathBuf>, Error> {
    let mut found: Vec<(bool, String, PathBuf)> = Vec::new();
    for path in files_under(root)? {
        let Ok(relative) = path.strip_prefix(root) else {
            continue;
        };
        let folders: Vec<String> = relative
            .iter()
            .map(|part| part.to_string_lossy().to_lowercase())
            .collect();
        let named = folders
            .last()
            .is_some_and(|file| *file == name.to_lowercase());
        let joined = folders.join("/");
        let not_x64 = ["win32", "opencv", "demo", "static"]
            .iter()
            .any(|word| joined.contains(word))
            || folders.iter().any(|folder| folder == "x86");
        if named && !not_x64 {
            let x64 = folders.iter().any(|folder| folder == "x64") || joined.contains("amd64");
            found.push((!x64, joined, path));
        }
    }
    found.sort();
    Ok(found.into_iter().next().map(|(_, _, path)| path))
}

/// Every file under `dir`.
fn files_under(dir: &Path) -> Result<Vec<PathBuf>, Error> {
    let mut files = Vec::new();
    for entry in fs::read_dir(dir).map_err(io(dir))? {
        let path = entry.map_err(io(dir))?.path();
        if path.is_dir() {
            files.extend(files_under(&path)?);
        } else {
            files.push(path);
        }
    }
    Ok(files)
}

/// Stages QHYCCD's SDK for Windows in the cache and returns the directory
/// that holds its x64 `qhyccd.lib`.
fn stage_qhy(runner: &mut impl Runner, host: &Host, offline: bool) -> Result<PathBuf, Error> {
    let extract = packaging::unpack_qhy(
        runner,
        &host.cache,
        &qhy::win64_archive(),
        qhy::SHA256_WIN64,
        &format!("qhy-sdk-{}-win64", qhy::VERSION),
        offline,
    )?;
    // Found rather than assumed: the archive's layout is QHYCCD's.
    let lib = pick_x64(&extract, "qhyccd.lib")?
        .ok_or_else(|| refused(format!("qhyccd.lib not found under {}", extract.display())))?;
    let dir = lib.parent().map_or_default(Path::to_path_buf);
    println!(
        "QHYCCD SDK {} staged: QHYCCD_SDK_DIR={}",
        qhy::VERSION,
        dir.display()
    );
    Ok(dir)
}

/// Whether `name` is the SDK for Windows that each of ZWO's downloads holds
/// beside the others: `ASI_Windows_SDK_V1.41.zip`.
fn is_windows_sdk(name: &str) -> bool {
    let zip = Path::new(name)
        .extension()
        .is_some_and(|extension| extension.eq_ignore_ascii_case("zip"));
    let name = name.to_lowercase();
    zip && name
        .find("windows")
        .and_then(|at| name.get(at..))
        .is_some_and(|rest| rest.contains("sdk"))
}

/// Stages ZWO's import libraries and DLLs in the cache, unless a previous run
/// did, and returns their directory, which is both the link's and `WiX`'s.
fn stage_zwo(
    runner: &mut impl Runner,
    root: &Path,
    host: &Host,
    offline: bool,
) -> Result<PathBuf, Error> {
    let dir = host.cache.join("zwo-win64");
    let lib = dir.join("lib");
    if !ZWO_FILES.iter().all(|(_, file)| lib.join(file).is_file()) {
        if offline {
            return Err(packaging::missing(&lib).into());
        }
        let module = packaging::ZWO_MODULE;
        let camera = packaging::module_const(root, module, "CAMERA_SDK_URL")?;
        let eaf = packaging::module_const(root, module, "EAF_SDK_URL")?;
        fs::create_dir_all(&lib).map_err(io(&lib))?;
        let work = tempfile::tempdir_in(&dir).map_err(io(&dir))?;
        for (url, zip, unpacked) in [(&camera, "asi.zip", "asi"), (&eaf, "eaf.zip", "eaf")] {
            let zip = dir.join(zip);
            packaging::download(runner, url, &zip)?;
            packaging::expand_zip(runner, &zip, &work.path().join(unpacked))?;
        }
        for inner in files_under(work.path())? {
            let name = inner.file_name().unwrap_or_default().to_string_lossy();
            if is_windows_sdk(&name) {
                let stem = inner.file_stem().unwrap_or_default().to_string_lossy();
                let nested = work.path().join(format!("nested-{stem}"));
                packaging::expand_zip(runner, &inner, &nested)?;
            }
        }
        for (file, staged) in ZWO_FILES {
            let found = pick_x64(work.path(), file)?
                .ok_or_else(|| refused(format!("{file} not found in the ZWO Windows SDK")))?;
            let to = lib.join(staged);
            fs::copy(&found, &to).map_err(io(&found))?;
            println!("build-msi: staged {} -> {staged}", found.display());
        }
    }
    println!("ZWO SDK staged: ZWO_SDK_LIB_DIR={}", lib.display());
    Ok(lib)
}

fn cargo_build(root: &Path, qhy: &Path, zwo: &Path, packages: &[String]) -> Cmd {
    let mut args = vec!["build", "--release"];
    args.extend(packages.iter().map(String::as_str));
    let skip: Vec<OsString> = SKIP_NATIVE_LINK.iter().map(OsString::from).collect();
    // CRT-static, so the exes need no VC++ redistributable.
    let cmd = Cmd::new("cargo", &args).dir(root).env_remove(&skip);
    packaging::rustflags(cmd, "-C target-feature=+crt-static")
        .env("QHYCCD_SDK_DIR", &qhy.to_string_lossy())
        .env("ZWO_SDK_LIB_DIR", &zwo.to_string_lossy())
}

/// The `wix` to run: the one on `PATH` when it is [`WIX_VERSION`], or that
/// version installed as a global .NET tool.
fn wix(runner: &mut impl Runner, host: &Host) -> Result<String, Error> {
    if let Ok(version) = runner.read(&Cmd::new("wix", &["--version"])) {
        let version = version.trim();
        if version.starts_with(WIX_VERSION) {
            return Ok("wix".to_owned());
        }
        println!("build-msi: wix on PATH is '{version}', need {WIX_VERSION}");
    }
    println!("Installing the wix CLI ({WIX_VERSION})");
    let tool = |verb: &str| {
        Cmd::new(
            "dotnet",
            &["tool", verb, "--global", "wix", "--version", WIX_VERSION],
        )
    };
    if runner.run(&tool("install")).is_err() {
        // A global tool at another version is updated instead.
        runner.run(&tool("update"))?;
    }
    // Named by its path: the tools directory may come late on PATH, or not
    // at all, and another wix would win.
    Ok(host
        .dotnet_tools
        .join("wix.exe")
        .to_string_lossy()
        .into_owned())
}

/// The `WiX` sources: `Package.wxs` and every fragment, by name.
fn sources(root: &Path) -> Result<Vec<PathBuf>, Error> {
    let fragments = root.join("installer").join("fragments");
    let mut names: Vec<String> = fs::read_dir(&fragments)
        .map_err(io(&fragments))?
        .filter_map(Result::ok)
        .filter_map(|entry| entry.file_name().into_string().ok())
        .filter(|name| {
            Path::new(name)
                .extension()
                .is_some_and(|extension| extension == "wxs")
        })
        .collect();
    names.sort();
    let mut sources = vec![Path::new("installer").join("Package.wxs")];
    sources.extend(
        names
            .iter()
            .map(|name| Path::new("installer").join("fragments").join(name)),
    );
    Ok(sources)
}

/// Builds the MSI and returns the directory it is in.
///
/// # Errors
///
/// When the version is refused, the toolchain or .NET is missing, an SDK
/// cannot be staged, or a command or file operation fails.
pub fn build(
    runner: &mut impl Runner,
    root: &Path,
    host: &Host,
    request: &Request,
) -> Result<PathBuf, Error> {
    let workspace = packaging::workspace_version(root)?;
    let version = packaging::stamp(&workspace, request.version.as_deref())?;
    let (product, full) = versions(&workspace, &version)?;
    if full != workspace {
        println!("build-msi: nightly stamp {full} (ProductVersion {product})");
    }
    if !host.dotnet {
        return Err(refused("dotnet not found (the wix CLI is a .NET tool)"));
    }
    let triple = runner.read(&Cmd::new("rustc", &["-vV"]))?;
    let triple = triple
        .lines()
        .find_map(|line| line.strip_prefix("host: "))
        .unwrap_or_default();
    // qhy-camera's delay-load link arguments are MSVC's: another toolchain
    // would drop its missing-DLL protection without a word.
    if triple != "x86_64-pc-windows-msvc" {
        return Err(refused(format!(
            "host toolchain must be x86_64-pc-windows-msvc (got: {triple})"
        )));
    }
    let services: Vec<String> = packaging::packaged(root)?
        .into_iter()
        .filter(|service| service != packaging::NO_WINDOWS_SDK)
        .collect();

    fs::create_dir_all(&host.cache).map_err(io(&host.cache))?;
    let offline = request.skip_sdk_staging;
    let qhy = stage_qhy(runner, host, offline)?;
    let zwo = stage_zwo(runner, root, host, offline)?;

    if !request.skip_build {
        for packages in packaging::cargo_runs(&services) {
            println!("Building release binaries: {}", packages.join(" "));
            runner.run(&cargo_build(root, &qhy, &zwo, &packages))?;
        }
    }
    let release = Path::new("target").join("release");
    for binary in packaging::binaries(&services) {
        let exe = release.join(format!("{binary}.exe"));
        if !root.join(&exe).is_file() {
            return Err(refused(format!(
                "{} missing after the build",
                exe.display()
            )));
        }
    }

    let wix = wix(runner, host)?;
    for extension in WIX_EXTENSIONS {
        let extension = format!("{extension}/{WIX_VERSION}");
        runner.run(&Cmd::new(&wix, &["extension", "add", "-g", &extension]))?;
    }
    let dist = root.join("dist").join(&full);
    fs::create_dir_all(&dist).map_err(io(&dist))?;
    let msi = dist.join(format!("rusty-photon-{full}-x64.msi"));
    println!("wix build -> {}", msi.display());
    let product = format!("Version={product}");
    let full = format!("FullVersion={full}");
    let bin = format!("bin={}", release.display());
    let zwo = format!("zwo={}", zwo.display());
    let out = msi.to_string_lossy();
    // -sw1149: the advisory WiX gives the native ServiceConfig element, the
    // one way to set the failure-actions flag, which util:ServiceConfig
    // cannot; verify-msi proves the flag works (installer/README.md). It is
    // one argument: `-sw 1149` would read 1149 as a source file.
    let mut args = vec![
        "build", "-arch", "x64", "-sw1149", "-d", &product, "-d", &full,
    ];
    for extension in WIX_EXTENSIONS {
        args.extend(["-ext", extension]);
    }
    args.extend([
        "-bindpath",
        &bin,
        "-bindpath",
        &zwo,
        "-bindpath",
        "repo=.",
        "-out",
        &out,
    ]);
    let sources: Vec<String> = sources(root)?
        .iter()
        .map(|source| source.to_string_lossy().into_owned())
        .collect();
    args.extend(sources.iter().map(String::as_str));
    runner.run(&Cmd::new(&wix, &args).dir(root))?;
    packaging::write_sums(&dist, &[".msi"])?;
    Ok(dist)
}

// The tools' tests compare Unix paths; the MSI's build runs on Windows.
#[cfg(test)]
#[cfg(unix)]
mod tests {
    use std::cell::RefCell;
    use std::rc::Rc;

    use super::*;
    use crate::process::{Fake, Script};

    const CAMERA_URL: &str =
        "https://dl.zwoastro.com/software?app=DeveloperCameraSdk&platform=windows86&region=Overseas";
    const EAF_URL: &str =
        "https://dl.zwoastro.com/software?app=DeveloperEafSdk&platform=windows86&region=Overseas";

    /// A repository and a Windows machine: `services` packaged, svbony-camera
    /// too, two `WiX` fragments, install-zwo-sdk's links, built binaries, and a
    /// cache holding both SDKs, with .NET.
    struct Fixture {
        root: tempfile::TempDir,
        cache_dir: tempfile::TempDir,
        host: Host,
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
        let fragments = r.join("installer/fragments");
        fs::create_dir_all(&fragments).unwrap();
        fs::write(r.join("installer/Package.wxs"), "").unwrap();
        for file in ["sentinel.wxs", "filemonitor.wxs", "README.md"] {
            fs::write(fragments.join(file), "").unwrap();
        }
        let module = r.join(packaging::ZWO_MODULE);
        fs::create_dir_all(module.parent().unwrap()).unwrap();
        fs::write(
            &module,
            format!(
                "export const CAMERA_SDK_URL = '{CAMERA_URL}';\nexport const EAF_SDK_URL = '{EAF_URL}';\n"
            ),
        )
        .unwrap();
        let release = r.join("target/release");
        fs::create_dir_all(&release).unwrap();
        for binary in
            packaging::binaries(&services.iter().map(|&s| s.to_owned()).collect::<Vec<_>>())
        {
            fs::write(release.join(format!("{binary}.exe")), "").unwrap();
        }

        let cache_dir = tempfile::tempdir().unwrap();
        let cache = cache_dir.path().to_path_buf();
        let qhy = cache.join(format!(
            "qhy-sdk-{v}-win64/sdk_win64_{v}/x64",
            v = qhy::VERSION
        ));
        fs::create_dir_all(&qhy).unwrap();
        fs::write(qhy.join("qhyccd.lib"), "lib").unwrap();
        let zwo = cache.join("zwo-win64/lib");
        fs::create_dir_all(&zwo).unwrap();
        for (_, file) in ZWO_FILES {
            fs::write(zwo.join(file), file).unwrap();
        }
        let host = Host {
            cache,
            dotnet: true,
            dotnet_tools: PathBuf::from("/home/ci/.dotnet/tools"),
        };
        Fixture {
            root,
            cache_dir,
            host,
        }
    }

    impl Fixture {
        /// The commands, with the fixture's paths shortened to `<root>` and
        /// `<cache>`.
        fn lines(&self, lines: &[String]) -> Vec<String> {
            let root = self.root.path().display().to_string();
            let cache = self.cache_dir.path().display().to_string();
            lines
                .iter()
                .map(|line| line.replace(&root, "<root>").replace(&cache, "<cache>"))
                .collect()
        }
    }

    const MSVC: &str = "rustc 1.99.0\nhost: x86_64-pc-windows-msvc\nrelease: 1.99.0\n";

    /// The cargo runs' variables, the cache shortened to `<cache>`.
    fn env() -> String {
        format!(
            "RUSTFLAGS=-C target-feature=+crt-static \
             QHYCCD_SDK_DIR=<cache>/qhy-sdk-{v}-win64/sdk_win64_{v}/x64 \
             ZWO_SDK_LIB_DIR=<cache>/zwo-win64/lib",
            v = qhy::VERSION
        )
    }

    fn wix_build(version: &str, full: &str) -> String {
        format!(
            "wix build -arch x64 -sw1149 -d Version={version} -d FullVersion={full} \
             -ext WixToolset.Util.wixext -ext WixToolset.Firewall.wixext \
             -ext WixToolset.UI.wixext -bindpath bin=target/release \
             -bindpath zwo=<cache>/zwo-win64/lib -bindpath repo=. \
             -out <root>/dist/{full}/rusty-photon-{full}-x64.msi installer/Package.wxs \
             installer/fragments/filemonitor.wxs installer/fragments/sentinel.wxs"
        )
    }

    #[test]
    fn builds_every_service_but_svbony_camera_crt_static_and_runs_wix() {
        let fixture = fixture(&[
            "filemonitor",
            "qhy-camera",
            "sentinel",
            "zwo-camera",
            "zwo-focuser",
        ]);
        let dist = fixture.root.path().join("dist/0.1.0");
        fs::create_dir_all(&dist).unwrap();
        fs::write(dist.join("rusty-photon-0.1.0-x64.msi"), "msi").unwrap();
        fs::write(dist.join("leftover.deb"), "deb").unwrap();
        let mut runner = Fake::answering(&[MSVC, "5.0.2+aa65968\n"]);
        let built = build(
            &mut runner,
            fixture.root.path(),
            &fixture.host,
            &Request::default(),
        )
        .unwrap();
        assert_eq!(built, dist);
        let env = env();
        assert_eq!(
            fixture.lines(&runner.lines()),
            [
                "rustc -vV".to_owned(),
                format!("{env} cargo build --release -p filemonitor -p qhy-camera -p sentinel -p doctor"),
                format!("{env} cargo build --release -p zwo-camera"),
                format!("{env} cargo build --release -p zwo-focuser"),
                "wix --version".to_owned(),
                "wix extension add -g WixToolset.Util.wixext/5.0.2".to_owned(),
                "wix extension add -g WixToolset.Firewall.wixext/5.0.2".to_owned(),
                "wix extension add -g WixToolset.UI.wixext/5.0.2".to_owned(),
                wix_build("0.1.0", "0.1.0"),
            ]
        );
        for cmd in runner.ran.iter().filter(|cmd| cmd.program == "cargo") {
            assert_eq!(cmd.dir.as_deref(), Some(fixture.root.path()));
            assert_eq!(
                cmd.env_remove,
                [
                    "QHYCCD_SKIP_NATIVE_LINK",
                    "ZWO_SKIP_NATIVE_LINK",
                    "CARGO_ENCODED_RUSTFLAGS"
                ]
            );
        }
        let wix = runner.ran.last().unwrap();
        assert_eq!(wix.dir.as_deref(), Some(fixture.root.path()));
        assert_eq!(
            fs::read_to_string(dist.join("SHA256SUMS.txt")).unwrap(),
            format!(
                "{}  rusty-photon-0.1.0-x64.msi\n",
                packaging::sha256(&dist.join("rusty-photon-0.1.0-x64.msi")).unwrap()
            )
        );
    }

    #[test]
    fn a_nightly_names_the_msi_by_its_stamp_and_versions_it_by_its_day() {
        let fixture = fixture(&["filemonitor"]);
        let mut runner = Fake::answering(&[MSVC, "5.0.2\n"]);
        let request = Request {
            skip_build: true,
            version: Some("0.1.0+nightly.202610220507.gabc1234".to_owned()),
            ..Request::default()
        };
        let dist = build(&mut runner, fixture.root.path(), &fixture.host, &request).unwrap();
        let full = "0.1.0+nightly.202610220507.gabc1234";
        assert_eq!(dist, fixture.root.path().join("dist").join(full));
        let lines = fixture.lines(&runner.lines());
        assert_eq!(lines.last().unwrap(), &wix_build("0.1.0.26295", full));
    }

    #[test]
    fn a_productversion_takes_the_stamps_year_and_day_of_the_year() {
        let nightly = |stamp: &str| {
            versions("0.1.0", &format!("0.1.0+nightly.{stamp}.gabcdef0"))
                .unwrap()
                .0
        };
        assert_eq!(
            versions("0.1.0", "0.1.0").unwrap(),
            ("0.1.0".to_owned(), "0.1.0".to_owned())
        );
        assert_eq!(nightly("202601010000"), "0.1.0.26001");
        assert_eq!(nightly("202703012359"), "0.1.0.27060");
        assert_eq!(nightly("202803010000"), "0.1.0.28061");
        assert_eq!(nightly("202802291200"), "0.1.0.28060");
        assert_eq!(nightly("202812310000"), "0.1.0.28366");
        assert_eq!(nightly("200012310000"), "0.1.0.366");
        assert_eq!(nightly("210012310000"), "0.1.0.365");
        assert_eq!(nightly("206512310000"), "0.1.0.65365");
    }

    #[test]
    fn refuses_a_version_an_msi_cannot_carry() {
        let refusal = |workspace: &str, version: &str| match versions(workspace, version) {
            Err(Error::Refused(reason)) => reason,
            other => panic!("{version}: {other:?}"),
        };
        for version in [
            "0.1.0+nightly.20261022050.gabc1234",
            "0.1.0+nightly.2026102205071.gabc1234",
            "0.1.0+nightly.202610220507.gabc123",
            "0.1.0+nightly.202610220507.gABC1234",
            "0.1.0+nightly.202610220507.g",
            "0.1.0+nightly.202610220507",
            "0.1.0+nightly.202610220507.habc1234",
            &format!("0.1.0+nightly.202610220507.g{}", "a".repeat(41)),
        ] {
            assert_eq!(
                refusal("0.1.0", version),
                format!("--version '{version}' is not 0.1.0+nightly.<yyyymmddhhmm>.g<sha>")
            );
        }
        for stamp in [
            "202613010000",
            "202600010000",
            "202610000000",
            "202702290000",
            "202604310000",
            "202610222400",
            "202610222360",
        ] {
            assert_eq!(
                refusal("0.1.0", &format!("0.1.0+nightly.{stamp}.gabc1234")),
                format!("--version stamp '{stamp}' is not a real UTC date-time (yyyymmddhhmm)")
            );
        }
        assert_eq!(
            refusal("0.1.0", "0.1.0+nightly.206601010000.gabc1234"),
            "nightly date field 66001 exceeds the MSI's 65535 per-field cap"
        );
        for workspace in ["0.1", "0.1.0.1", "0.1.x", "0..1"] {
            assert_eq!(
                refusal(workspace, workspace),
                format!("workspace version '{workspace}' is not a plain x.y.z (the MSI's ProductVersion base)")
            );
        }
    }

    #[test]
    fn refuses_a_machine_it_cannot_build_on() {
        check_host("windows", "x86_64").unwrap();
        for (os, arch) in [
            ("linux", "x86_64"),
            ("windows", "aarch64"),
            ("macos", "aarch64"),
        ] {
            let error = check_host(os, arch).unwrap_err();
            assert!(error.contains(&format!("not {arch} {os}")), "{error}");
        }
        let fixture = fixture(&["filemonitor"]);
        let gnu = "host: x86_64-pc-windows-gnu\n";
        let error = build(
            &mut Fake::answering(&[gnu]),
            fixture.root.path(),
            &fixture.host,
            &Request::default(),
        )
        .unwrap_err();
        assert_eq!(
            error.to_string(),
            "host toolchain must be x86_64-pc-windows-msvc (got: x86_64-pc-windows-gnu)"
        );
        let host = Host {
            dotnet: false,
            ..fixture.host.clone()
        };
        let mut runner = Fake::default();
        let error =
            build(&mut runner, fixture.root.path(), &host, &Request::default()).unwrap_err();
        assert_eq!(
            error.to_string(),
            "dotnet not found (the wix CLI is a .NET tool)"
        );
        assert!(runner.ran.is_empty(), "{:?}", runner.lines());
    }

    #[test]
    fn refuses_to_package_a_binary_the_build_did_not_make() {
        let fixture = fixture(&["filemonitor", "sentinel"]);
        fs::remove_file(fixture.root.path().join("target/release/doctor.exe")).unwrap();
        let mut runner = Fake::answering(&[MSVC]);
        let error = build(
            &mut runner,
            fixture.root.path(),
            &fixture.host,
            &Request::default(),
        )
        .unwrap_err();
        assert_eq!(
            error.to_string(),
            "target/release/doctor.exe missing after the build"
        );
        assert!(
            !runner.lines().iter().any(|line| line.starts_with("wix")),
            "{:?}",
            runner.lines()
        );
    }

    #[test]
    fn installs_the_pinned_wix_when_path_has_another_or_none() {
        let fixture = fixture(&["filemonitor"]);
        let request = Request {
            skip_build: true,
            ..Request::default()
        };
        // Another version on PATH: installed, or updated where installed.
        let mut runner = Fake {
            fail_at: Some(2),
            ..Fake::answering(&[MSVC, "4.0.5\n"])
        };
        build(&mut runner, fixture.root.path(), &fixture.host, &request).unwrap();
        let lines = fixture.lines(&runner.lines());
        assert_eq!(
            lines[1..6],
            [
                "wix --version",
                "dotnet tool install --global wix --version 5.0.2",
                "dotnet tool update --global wix --version 5.0.2",
                "/home/ci/.dotnet/tools/wix.exe extension add -g WixToolset.Util.wixext/5.0.2",
                "/home/ci/.dotnet/tools/wix.exe extension add -g WixToolset.Firewall.wixext/5.0.2",
            ]
        );
        assert!(
            lines
                .last()
                .unwrap()
                .starts_with("/home/ci/.dotnet/tools/wix.exe build "),
            "{lines:?}"
        );
        // None on PATH.
        let mut runner = Fake {
            fail_at: Some(1),
            ..Fake::answering(&[MSVC])
        };
        build(&mut runner, fixture.root.path(), &fixture.host, &request).unwrap();
        let lines = runner.lines();
        assert_eq!(lines[2], "dotnet tool install --global wix --version 5.0.2");
        assert!(
            lines[3].starts_with("/home/ci/.dotnet/tools/wix.exe extension add"),
            "{lines:?}"
        );
        // An install and an update that both fail stop the build.
        let mut runner = Script::default()
            .on("rustc -vV", || Ok(MSVC.to_owned()))
            .on("wix --version", || Err(()))
            .on("dotnet tool", || Err(()));
        let error = build(&mut runner, fixture.root.path(), &fixture.host, &request).unwrap_err();
        assert!(
            error
                .to_string()
                .contains("dotnet tool update --global wix"),
            "{error}"
        );
    }

    #[test]
    fn rebuilds_offline_only_from_a_full_cache() {
        let fixture = fixture(&["filemonitor"]);
        fs::remove_file(
            fixture
                .cache_dir
                .path()
                .join("zwo-win64/lib/EAF_focuser.dll"),
        )
        .unwrap();
        let request = Request {
            skip_sdk_staging: true,
            ..Request::default()
        };
        let mut runner = Fake::answering(&[MSVC]);
        let error = build(&mut runner, fixture.root.path(), &fixture.host, &request).unwrap_err();
        assert!(
            error.to_string().contains("--skip-sdk-staging set"),
            "{error}"
        );
        assert_eq!(runner.lines(), ["rustc -vV"]);
        let qhy = fixture
            .cache_dir
            .path()
            .join(format!("qhy-sdk-{}-win64", qhy::VERSION));
        fs::remove_dir_all(qhy).unwrap();
        let error = build(
            &mut Fake::answering(&[MSVC]),
            fixture.root.path(),
            &fixture.host,
            &request,
        )
        .unwrap_err();
        assert!(
            error.to_string().contains("--skip-sdk-staging set"),
            "{error}"
        );
    }

    /// Creates `files` under `dir`, each holding its own path.
    fn lay_out(dir: &Path, files: &[&str]) {
        for file in files {
            let path = dir.join(file);
            fs::create_dir_all(path.parent().unwrap()).unwrap();
            fs::write(path, file).unwrap();
        }
    }

    /// What ZWO's two downloads unpack to, the SDKs for Windows inside them
    /// unpacked too, as their 2026-10 releases lay them out.
    fn zwo_downloads(dest: &Path, zip: &str) {
        match zip {
            "asi.zip" => lay_out(
                dest,
                &[
                    "ASI_Camera_SDK/ASI_Windows_SDK_V1.41.zip",
                    "ASI_Camera_SDK/ASISDK_ANDROID.zip",
                ],
            ),
            "eaf.zip" => lay_out(dest, &["EAF_SDK_V1.8.1/EAF_Windows_SDK_V1.8.1.zip"]),
            "ASI_Windows_SDK_V1.41.zip" => lay_out(
                dest,
                &[
                    "ASI SDK/demo/opencv2/bin/ASICamera2.dll",
                    "ASI SDK/lib/x86/ASICamera2.dll",
                    "ASI SDK/lib/x86/ASICamera2.lib",
                    "ASI SDK/lib/x64/ASICamera2.dll",
                    "ASI SDK/lib/x64/ASICamera2.lib",
                ],
            ),
            "EAF_Windows_SDK_V1.8.1.zip" => lay_out(
                dest,
                &[
                    "EAF_Windows_SDK_V1.8.1/lib/Windows/Win32/Release/EAF_focuser.dll",
                    "EAF_Windows_SDK_V1.8.1/lib/Windows/Win32/Release/EAF_focuser.lib",
                    "EAF_Windows_SDK_V1.8.1/lib/Windows7/x64/Release/EAF_focuser.dll",
                    "EAF_Windows_SDK_V1.8.1/lib/Windows7/x64/Release/EAF_focuser.lib",
                    "EAF_Windows_SDK_V1.8.1/lib/Windows/x64/Release/EAF_focuser.dll",
                    "EAF_Windows_SDK_V1.8.1/lib/Windows/x64/Release/EAF_focuser.lib",
                    "EAF_Windows_SDK_V1.8.1/lib/Windows/x64/Release/EAF_focuser-static.lib",
                ],
            ),
            other => panic!("unexpected zip {other}"),
        }
    }

    /// A Windows box with nothing cached: curl downloads write their file,
    /// and Expand-Archive unpacks what [`zwo_downloads`] says.
    fn downloading() -> (Script, Rc<RefCell<Vec<String>>>) {
        let expanded = Rc::new(RefCell::new(Vec::new()));
        let seen = Rc::clone(&expanded);
        let script = Script::default()
            .on("rustc -vV", || Ok(MSVC.to_owned()))
            .on("wix --version", || Ok("5.0.2\n".to_owned()))
            .on_cmd("curl", |cmd| {
                fs::write(&cmd.args[2], "zip").unwrap();
                Ok(String::new())
            })
            .on_cmd("powershell", move |cmd| {
                let var = |name: &str| {
                    cmd.env
                        .iter()
                        .find(|(set, _)| set == name)
                        .map(|(_, value)| PathBuf::from(value))
                        .unwrap()
                };
                let zip = var("RP_ZIP");
                let name = zip.file_name().unwrap().to_string_lossy().into_owned();
                zwo_downloads(&var("RP_DEST"), &name);
                seen.borrow_mut().push(name);
                Ok(String::new())
            });
        (script, expanded)
    }

    #[test]
    fn stages_the_x64_zwo_sdks_from_install_zwo_sdks_links() {
        let fixture = fixture(&["filemonitor"]);
        let lib = fixture.cache_dir.path().join("zwo-win64/lib");
        fs::remove_dir_all(&lib).unwrap();
        let (mut runner, expanded) = downloading();
        let request = Request {
            skip_build: true,
            ..Request::default()
        };
        build(&mut runner, fixture.root.path(), &fixture.host, &request).unwrap();
        let lines = fixture.lines(&runner.lines);
        let zip = |name: &str| format!("<cache>/zwo-win64/{name}");
        assert_eq!(
            lines[1..7],
            [
                format!("curl -fsSL -o {}.part {CAMERA_URL}", zip("asi.zip")),
                "powershell -NoProfile -NonInteractive -Command $ErrorActionPreference = 'Stop'; \
                 $ProgressPreference = 'SilentlyContinue'; Expand-Archive -LiteralPath $env:RP_ZIP \
                 -DestinationPath $env:RP_DEST -Force"
                    .to_owned(),
                format!("curl -fsSL -o {}.part {EAF_URL}", zip("eaf.zip")),
                lines[2].clone(),
                lines[2].clone(),
                lines[2].clone(),
            ]
        );
        let mut expanded = expanded.borrow().clone();
        expanded.sort();
        assert_eq!(
            expanded,
            [
                "ASI_Windows_SDK_V1.41.zip",
                "EAF_Windows_SDK_V1.8.1.zip",
                "asi.zip",
                "eaf.zip"
            ]
        );
        let staged = |file: &str| fs::read_to_string(lib.join(file)).unwrap();
        assert_eq!(staged("ASICamera2.lib"), "ASI SDK/lib/x64/ASICamera2.lib");
        assert_eq!(staged("ASICamera2.dll"), "ASI SDK/lib/x64/ASICamera2.dll");
        assert_eq!(
            staged("EAFFocuser.lib"),
            "EAF_Windows_SDK_V1.8.1/lib/Windows/x64/Release/EAF_focuser.lib"
        );
        assert_eq!(
            staged("EAF_focuser.dll"),
            "EAF_Windows_SDK_V1.8.1/lib/Windows/x64/Release/EAF_focuser.dll"
        );
        let left: Vec<String> = fs::read_dir(lib.parent().unwrap())
            .unwrap()
            .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        let mut left = left;
        left.sort();
        assert_eq!(
            left,
            ["asi.zip", "eaf.zip", "lib"],
            "the scratch unpack is gone"
        );
    }

    #[test]
    fn stages_qhyccds_windows_sdk_from_its_pinned_archive() {
        let fixture = fixture(&["filemonitor"]);
        let cache = fixture.cache_dir.path();
        fs::remove_dir_all(cache.join(format!("qhy-sdk-{}-win64", qhy::VERSION))).unwrap();
        let (runner, _) = downloading();
        let mut runner = runner;
        let error = build(
            &mut runner,
            fixture.root.path(),
            &fixture.host,
            &Request::default(),
        )
        .unwrap_err();
        // The download is not QHYCCD's archive, so its SHA-256 is another.
        assert!(
            matches!(
                error,
                Error::Packaging(packaging::Error::Sha256Mismatch { .. })
            ),
            "{error}"
        );
        assert_eq!(
            fixture.lines(&runner.lines)[1],
            format!(
                "curl -fsSL -o <cache>/sdk_win64_{v}.zip.part \
                 https://www.qhyccd.com/file/repository/publish/SDK/260604/sdk_win64_{v}.zip",
                v = qhy::VERSION
            )
        );
    }

    #[test]
    fn picks_the_x64_build_of_a_library() {
        let dir = tempfile::tempdir().unwrap();
        lay_out(
            dir.path(),
            &[
                "b/Win32/qhyccd.lib",
                "b/x86/QHYCCD.LIB",
                "a/demo/qhyccd.lib",
                "a/static/qhyccd.lib",
                "c/lib/qhyccd.lib",
                "z/amd64/qhyccd.lib",
            ],
        );
        let pick = |name: &str| {
            pick_x64(dir.path(), name)
                .unwrap()
                .map(|path| path.strip_prefix(dir.path()).unwrap().display().to_string())
        };
        assert_eq!(pick("qhyccd.lib").as_deref(), Some("z/amd64/qhyccd.lib"));
        fs::remove_file(dir.path().join("z/amd64/qhyccd.lib")).unwrap();
        // Every other build sorts before it, and each is passed over.
        assert_eq!(pick("QhyCcd.lib").as_deref(), Some("c/lib/qhyccd.lib"));
        assert_eq!(pick("other.lib"), None);
    }

    #[test]
    fn knows_the_sdk_for_windows_in_a_zwo_download() {
        for name in [
            "ASI_Windows_SDK_V1.41.zip",
            "EAF_Windows_SDK_V1.8.1.ZIP",
            "windows-sdk.zip",
        ] {
            assert!(is_windows_sdk(name), "{name}");
        }
        for name in [
            "ASISDK_ANDROID.zip",
            "ASI_linux_mac_SDK_V1.41.tar.bz2",
            "SDK_Windows.zip",
            "EAF_Windows_SDK_V1.8.1.zip.txt",
        ] {
            assert!(!is_windows_sdk(name), "{name}");
        }
    }
}
