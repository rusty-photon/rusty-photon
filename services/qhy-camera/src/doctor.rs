//! `qhy-camera doctor` — the per-service doctor subcommand: read-only
//! diagnosis of this service's own config plus what the QHYCCD SDK can see.
//!
//! The contract is docs/services/doctor.md §Per-service doctors'. On Windows
//! real-SDK builds it additionally carries the QHYCCD installation
//! diagnostics: how the delay-loaded `qhyccd.dll` resolves
//! (`hardware.sdk-dll`), and the loaded SDK version vs. the pinned
//! build-time version (`hardware.sdk-version` — ABI skew made visible,
//! ADR-015 accepted risk), with All-in-One driver-pack presence and the
//! download URL in the failing check's suggestion. Behavioral contracts
//! DR1–DR5 in `docs/services/qhy-camera.md` § "Windows: qhyccd.dll
//! resolution".
//!
//! Check assembly is pure over plain data so it is unit-testable on every
//! platform; only the gathering (real `LoadLibrary` + SDK calls) is
//! platform/build-gated.

use std::io::{BufRead, Write};
use std::path::{Path, PathBuf};
use std::process::exit;

use rusty_photon_doctor_checks::claims::{block_entries, devices_report, usb_devices_checks};
use rusty_photon_doctor_checks::report::Check;
#[cfg(any(windows, test))]
use rusty_photon_doctor_checks::report::Status;
use rusty_photon_doctor_checks::service::{self, SdkOutcome};

use crate::claims::{self, listing, UsbSource, NORMALIZER, SERVICE};
#[cfg(any(all(windows, not(feature = "simulation")), test))]
use crate::preflight::PINNED_SDK_VERSION;
use crate::preflight::{PinnedSdkVersion, QHY_ALL_IN_ONE_URL};
use crate::{load_effective_config, read_effective_config, CliOverrides};

/// The SDK version reported by the loaded DLL (`GetQHYCCDSDKVersion`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SdkVersionFinding {
    /// `GetQHYCCDSDKVersion` succeeded.
    Loaded {
        year: u32,
        month: u32,
        day: u32,
        subday: u32,
    },
    /// The DLL resolved but SDK init / version query failed.
    Failed(String),
}

impl SdkVersionFinding {
    /// `true` when a loaded version differs from the build-time pin
    /// (year/month/day; a non-zero subday alone is not skew).
    #[must_use]
    pub fn skewed(&self, pinned: &PinnedSdkVersion) -> bool {
        match self {
            Self::Loaded {
                year, month, day, ..
            } => (*year, *month, *day) != (pinned.year, pinned.month, pinned.day),
            Self::Failed(_) => false,
        }
    }

    #[cfg(any(all(windows, not(feature = "simulation")), test))]
    fn render_loaded(year: u32, month: u32, day: u32, subday: u32) -> String {
        if subday == 0 {
            format!("{year:02}.{month:02}.{day:02}")
        } else {
            format!("{year:02}.{month:02}.{day:02}.{subday}")
        }
    }
}

/// What the platform/build-specific gather produced: the standard SDK
/// enumeration outcome (`None` only when the delay-loaded DLL is missing —
/// the SDK was never queried, and `hardware.sdk-dll` carries the whole
/// story), plus the Windows-only installation checks.
struct Probe {
    sdk: Option<SdkOutcome>,
    /// The cameras that enumeration found, id and model, for the
    /// `usb-devices.*` checks; `None` whenever `sdk` is not a list of devices.
    cameras: Option<Vec<claims::Enumerated>>,
    extras: Vec<Check>,
}

/// `doctor --devices`: the QHY cameras on the bus by USB port, and the
/// `usb_devices` block to paste (docs/services/qhy-camera.md U8).
///
/// Read-only and enumeration-only — each camera's id and model are read
/// without opening it, so no CFW probe runs and no wheel can be seen. Its
/// `Sdk::new` still starts the SDK and scans in this process, beside any
/// service holding the cameras, which is unmeasured. Exits 0; 1 when the
/// config cannot be read, its list breaks a rule (the ports are listed all the
/// same — they are how the list gets fixed), the scan failed, or the SDK could
/// not enumerate; 2 when the config path cannot be resolved.
pub fn run_devices(config: Option<PathBuf>, source: &UsbSource) -> ! {
    let config_path = match rusty_photon_config::resolve_config_path(SERVICE, config) {
        Ok(path) => path,
        Err(error) => {
            eprintln!("doctor: {error}");
            exit(2);
        }
    };
    match devices(&config_path, source) {
        Ok((output, code)) => {
            print!("{output}");
            exit(code);
        }
        Err(error) => {
            eprintln!("doctor: {error}");
            exit(1);
        }
    }
}

/// The listing [`run_devices`] prints, and its exit code; `Err` when the
/// config cannot be read at all, which goes to stderr as the path error does.
fn devices(
    config_path: &Path,
    source: &UsbSource,
) -> Result<(String, i32), Box<dyn std::error::Error + Send + Sync>> {
    let config = read_effective_config(config_path, &CliOverrides::default())?;
    let list = config.usb_devices.as_deref().map(block_entries);
    Ok(devices_report(
        NORMALIZER,
        SERVICE,
        config_path,
        config.check_list().map(|()| list.as_deref()),
        source.scan(),
        || {
            let cameras = enumerated()?;
            Ok((claims::sdk_cameras(&cameras), cameras))
        },
        |claims, cameras| listing(&config, claims, cameras),
    ))
}

/// What the SDK enumerates, ids and models, before any open. A Windows
/// real-SDK build resolves the delay-loaded `qhyccd.dll` first: an SDK call
/// without it faults instead of returning an error.
fn enumerated() -> Result<Vec<claims::Enumerated>, String> {
    #[cfg(all(windows, not(feature = "simulation")))]
    crate::preflight::ensure_qhyccd_dll().map_err(|error| error.to_string())?;
    let sdk = qhyccd_rs::Sdk::new().map_err(|error| error.to_string())?;
    Ok(claims::enumerate(&sdk))
}

/// The `usb-devices.*` checks (U12): the claims a start would make from
/// `source`'s scan and the `cameras` the report's probe enumerated, with no
/// CFW probe, so nothing is opened. None — and no scan taken — when the
/// config did not load, which `config.full-shape` reports, or the SDK did not
/// enumerate, which `hardware.sdk-devices` (or `hardware.sdk-dll`) reports.
fn claims_checks(
    config: Option<crate::Config>,
    source: &UsbSource,
    cameras: Option<Vec<claims::Enumerated>>,
) -> Vec<Check> {
    let (Some(config), Some(cameras)) = (config, cameras) else {
        return Vec::new();
    };
    let list = config.usb_devices.as_deref().map(block_entries);
    usb_devices_checks(
        NORMALIZER,
        SERVICE,
        list.as_deref(),
        source.scan(),
        || Ok((claims::sdk_cameras(&cameras), cameras)),
        |claims, cameras| listing(&config, claims, cameras),
    )
}

/// The report: `config.full-shape`, `hardware.sdk-devices` and the Windows
/// installation checks, and the `usb-devices.*` checks of the claims a start
/// would make from `source`'s scan (U12).
pub fn run(config: Option<PathBuf>, json: bool, source: &UsbSource) -> ! {
    let config_path = match rusty_photon_config::resolve_config_path(SERVICE, config) {
        Ok(path) => path,
        Err(error) => {
            eprintln!("doctor: {error}");
            exit(2);
        }
    };
    // One config load and one SDK enumeration serve every check, so all of
    // them judge the same file and the same cameras.
    let loaded = load_effective_config(&config_path, &CliOverrides::default());
    let mut probe = probe();
    let cameras = probe.cameras.take();
    let mut checks = assemble(&config_path, &loaded, probe);
    checks.extend(claims_checks(loaded.ok(), source, cameras));
    // DR2: offer the download page only in operator-facing text mode, and
    // only when the Windows SDK installation needs attention. JSON mode is
    // machine mode (central doctor's shell-out) and must never prompt.
    #[cfg(windows)]
    let offer_download = !json && needs_operator_attention(&checks);
    let (output, code) = service::emit(
        SERVICE,
        env!("CARGO_PKG_VERSION"),
        &config_path,
        checks,
        json,
    );
    print!("{output}");
    // `exit` bypasses destructors: flush so the report is fully out before
    // any prompt, and cannot be lost when stdout is buffered.
    let _ = std::io::stdout().flush();
    #[cfg(windows)]
    if offer_download {
        let stdin = std::io::stdin();
        let mut stdout = std::io::stdout();
        if prompt_open_download_page(&mut stdin.lock(), &mut stdout) {
            open_download_page();
        }
        let _ = std::io::stdout().flush();
    }
    exit(code);
}

/// The full check list: the standard pair (config shape, SDK enumeration)
/// plus whatever installation checks the probe produced.
fn assemble<E: std::fmt::Display>(
    config_path: &Path,
    loaded: &Result<crate::Config, E>,
    probe: Probe,
) -> Vec<Check> {
    let mut checks = vec![service::full_shape_check(config_path, |_| {
        loaded.as_ref().map(|_| ()).map_err(ToString::to_string)
    })];
    if let Some(outcome) = probe.sdk {
        checks.push(service::sdk_devices_check(outcome));
    }
    checks.extend(probe.extras);
    checks
}

/// DR2's trigger, over the assembled checks: a failed DLL resolution or a
/// non-`ok` SDK version (skew warns, an unreadable version fails). Both
/// checks exist only on Windows real-SDK builds, so this is statically
/// `false` everywhere else.
#[cfg(any(windows, test))]
fn needs_operator_attention(checks: &[Check]) -> bool {
    checks.iter().any(|check| {
        (check.name == "hardware.sdk-dll" || check.name == "hardware.sdk-version")
            && check.status != Status::Ok
    })
}

/// `hardware.sdk-version`: loaded-vs-pinned. A skew is a warning (ADR-015
/// accepts the ABI-skew risk); an unreadable version is a failure — the
/// DLL resolved but the SDK is not usable, which is DR3's unhealthy case.
#[cfg(any(all(windows, not(feature = "simulation")), test))]
fn sdk_version_check(finding: &SdkVersionFinding) -> Check {
    match finding {
        SdkVersionFinding::Loaded {
            year,
            month,
            day,
            subday,
        } => {
            let loaded = SdkVersionFinding::render_loaded(*year, *month, *day, *subday);
            if finding.skewed(&PINNED_SDK_VERSION) {
                Check::warn(
                    "hardware.sdk-version",
                    None,
                    format!(
                        "loaded qhyccd.dll reports {loaded}, which differs from the \
                         build-time pin {PINNED_SDK_VERSION} (possible ABI skew)"
                    ),
                    Some(format!(
                        "if qhy-camera misbehaves, install the All-in-One pack matching \
                         the pinned SDK from {QHY_ALL_IN_ONE_URL}"
                    )),
                )
            } else {
                Check::ok(
                    "hardware.sdk-version",
                    None,
                    format!("{loaded} (matches the build-time pin {PINNED_SDK_VERSION})"),
                )
            }
        }
        SdkVersionFinding::Failed(reason) => Check::fail(
            "hardware.sdk-version",
            None,
            format!("qhyccd.dll resolved but the SDK is not usable: {reason}"),
            Some(format!(
                "install QHY's All-in-One pack from {QHY_ALL_IN_ONE_URL} (it provides \
                 both the signed device driver and qhyccd.dll)"
            )),
        ),
    }
}

/// `hardware.sdk-dll` for the missing-DLL case: every probed directory and
/// failed load attempt in the detail, the All-in-One remedy plus each
/// known driver-pack root's presence in the suggestion.
#[cfg(any(all(windows, not(feature = "simulation")), test))]
fn dll_missing_check(
    probed: &[PathBuf],
    failures: &[crate::preflight::LoadFailure],
    driver_pack: &[(PathBuf, bool)],
) -> Check {
    use std::fmt::Write as _;
    let mut detail = String::from(
        "qhyccd.dll was not found; probed, plus the standard Windows DLL \
         search order (exe directory, System32, PATH):",
    );
    for dir in probed {
        let _ = write!(detail, " {};", dir.display());
    }
    for failure in failures {
        let _ = write!(
            detail,
            " load failed {} — {};",
            failure.path.display(),
            failure.error
        );
    }
    let mut suggestion = format!(
        "install QHY's All-in-One pack from {QHY_ALL_IN_ONE_URL} (it provides both \
         the signed device driver and qhyccd.dll)"
    );
    for (dir, exists) in driver_pack {
        let presence = if *exists { "present" } else { "not found" };
        let _ = write!(suggestion, "; {} — {presence}", dir.display());
    }
    Check::fail("hardware.sdk-dll", None, detail, Some(suggestion))
}

// --- gathering: Windows real-SDK builds carry the DLL machinery ---------

#[cfg(all(windows, not(feature = "simulation")))]
fn probe() -> Probe {
    use crate::preflight::DllResolution;

    let driver_pack: Vec<(PathBuf, bool)> =
        crate::preflight::driver_pack_dirs(|var| std::env::var(var).ok())
            .into_iter()
            .map(|dir| {
                let exists = dir.is_dir();
                (dir, exists)
            })
            .collect();

    let found = match crate::preflight::resolve_and_load() {
        // Never call into the SDK when the delay-loaded DLL is missing —
        // the delay-load helper would fault instead of returning an error.
        DllResolution::NotFound { probed, failures } => {
            return Probe {
                sdk: None,
                cameras: None,
                extras: vec![dll_missing_check(&probed, &failures, &driver_pack)],
            };
        }
        DllResolution::FoundAt(dll) => format!("found — {}", dll.display()),
        DllResolution::FoundByName => "found via the default Windows DLL search \
                                       order (exe directory, System32, PATH)"
            .to_string(),
    };
    let dll_check = Check::ok("hardware.sdk-dll", None, found);
    let (version, outcome, cameras) = query_sdk();
    Probe {
        sdk: Some(outcome),
        cameras,
        extras: vec![dll_check, sdk_version_check(&version)],
    }
}

/// Init the SDK from the resident DLL, read `GetQHYCCDSDKVersion`, and
/// enumerate — ids for `hardware.sdk-devices`, ids and models for the claims.
/// First real FFI calls of the process: the delay-load helper binds them to
/// the module the preflight probe loaded.
#[cfg(all(windows, not(feature = "simulation")))]
fn query_sdk() -> (
    SdkVersionFinding,
    SdkOutcome,
    Option<Vec<claims::Enumerated>>,
) {
    match qhyccd_rs::Sdk::new() {
        Ok(sdk) => {
            let devices = SdkOutcome::Devices(
                sdk.cameras()
                    .map(|camera| camera.id().to_string())
                    .collect(),
            );
            let cameras = claims::enumerate(&sdk);
            let version = match sdk.version() {
                Ok(version) => SdkVersionFinding::Loaded {
                    year: version.year,
                    month: version.month,
                    day: version.day,
                    subday: version.subday,
                },
                Err(error) => {
                    SdkVersionFinding::Failed(format!("GetQHYCCDSDKVersion failed: {error}"))
                }
            };
            (version, devices, Some(cameras))
        }
        Err(error) => (
            SdkVersionFinding::Failed(format!("SDK init failed: {error}")),
            SdkOutcome::Error {
                detail: format!("SDK init failed: {error}"),
                suggestion: Some(format!(
                    "install QHY's All-in-One pack from {QHY_ALL_IN_ONE_URL}"
                )),
            },
            None,
        ),
    }
}

// --- gathering: simulation builds and Unix real builds ------------------
// The SDK is either simulated or linked statically at build time (DR4/DR5)
// — there is no DLL to resolve, so only the standard enumeration runs.

#[cfg(not(all(windows, not(feature = "simulation"))))]
fn probe() -> Probe {
    let (sdk, cameras) = match qhyccd_rs::Sdk::new() {
        Ok(sdk) => (
            SdkOutcome::Devices(
                sdk.cameras()
                    .map(|camera| camera.id().to_string())
                    .collect(),
            ),
            Some(claims::enumerate(&sdk)),
        ),
        Err(error) => (
            SdkOutcome::Error {
                detail: error.to_string(),
                suggestion: Some(
                    "check the USB connection and, on Linux, that \
                     rusty-photon-qhy-firmware-install has been run once as root"
                        .to_string(),
                ),
            },
            None,
        ),
    };
    Probe {
        sdk: Some(sdk),
        cameras,
        extras: Vec::new(),
    }
}

/// DR2: ask whether to open the QHY download page. Anything but an explicit
/// yes — including EOF on a non-interactive stdin — counts as "No".
pub fn prompt_open_download_page(input: &mut dyn BufRead, output: &mut dyn Write) -> bool {
    let _ = write!(
        output,
        "Open the QHY download page ({QHY_ALL_IN_ONE_URL}) in your browser? [y/N] "
    );
    let _ = output.flush();
    let mut line = String::new();
    if input.read_line(&mut line).is_err() {
        return false;
    }
    matches!(line.trim(), "y" | "Y" | "yes" | "Yes" | "YES")
}

/// Open the QHY download page in the default browser via `cmd /C start`.
/// The empty argument fills `start`'s window-title slot so the URL can never
/// be mistaken for one.
#[cfg(windows)]
fn open_download_page() {
    match std::process::Command::new("cmd")
        .args(["/C", "start", "", QHY_ALL_IN_ONE_URL])
        .spawn()
    {
        Ok(_) => println!("Opening {QHY_ALL_IN_ONE_URL} ..."),
        Err(error) => eprintln!("Could not open the browser: {error}"),
    }
}

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
mod tests {
    use super::*;
    use std::io::Cursor;

    fn loaded(year: u32, month: u32, day: u32, subday: u32) -> SdkVersionFinding {
        SdkVersionFinding::Loaded {
            year,
            month,
            day,
            subday,
        }
    }

    #[test]
    fn matching_version_is_ok_and_names_the_pin() {
        let check = sdk_version_check(&loaded(26, 6, 4, 0));
        assert_eq!(check.status, Status::Ok);
        assert!(
            check
                .detail
                .contains("26.06.04 (matches the build-time pin 26.06.04)"),
            "{}",
            check.detail
        );
    }

    #[test]
    fn version_skew_warns_and_points_at_the_matching_pack() {
        let finding = loaded(26, 9, 12, 0);
        assert!(finding.skewed(&PINNED_SDK_VERSION));
        let check = sdk_version_check(&finding);
        assert_eq!(check.status, Status::Warn);
        assert!(
            check.detail.contains("26.09.12") && check.detail.contains("differs"),
            "{}",
            check.detail
        );
        assert!(
            check
                .suggestion
                .as_deref()
                .unwrap()
                .contains(QHY_ALL_IN_ONE_URL),
            "{:?}",
            check.suggestion
        );
    }

    #[test]
    fn subday_alone_is_not_skew_and_renders_with_suffix() {
        let finding = loaded(26, 6, 4, 1);
        assert!(!finding.skewed(&PINNED_SDK_VERSION));
        assert_eq!(SdkVersionFinding::render_loaded(26, 6, 4, 1), "26.06.04.1");
        assert_eq!(sdk_version_check(&finding).status, Status::Ok);
    }

    #[test]
    fn unreadable_version_fails_with_the_all_in_one_remedy() {
        let check = sdk_version_check(&SdkVersionFinding::Failed(
            "SDK init failed: QHYCCD_ERROR".into(),
        ));
        assert_eq!(check.status, Status::Fail);
        assert!(
            check.detail.contains("SDK init failed: QHYCCD_ERROR"),
            "{}",
            check.detail
        );
        assert!(
            check
                .suggestion
                .as_deref()
                .unwrap()
                .contains(QHY_ALL_IN_ONE_URL),
            "{:?}",
            check.suggestion
        );
    }

    #[test]
    fn missing_dll_check_names_probed_dirs_failures_and_driver_pack_roots() {
        let check = dll_missing_check(
            &[
                PathBuf::from(r"C:\Program Files\rusty-photon"),
                PathBuf::from(r"C:\Program Files\QHYCCD\AllInOne\sdk\x64"),
            ],
            &[crate::preflight::LoadFailure {
                path: PathBuf::from(r"C:\Program Files\rusty-photon\qhyccd.dll"),
                error: "wrong architecture".to_string(),
            }],
            &[
                (PathBuf::from(r"C:\Program Files\QHYCCD"), true),
                (PathBuf::from(r"C:\Program Files (x86)\QHYCCD"), false),
            ],
        );
        assert_eq!(check.status, Status::Fail);
        assert!(
            check.detail.contains(r"C:\Program Files\rusty-photon;"),
            "{}",
            check.detail
        );
        assert!(
            check.detail.contains(
                r"load failed C:\Program Files\rusty-photon\qhyccd.dll — wrong architecture"
            ),
            "{}",
            check.detail
        );
        let suggestion = check.suggestion.as_deref().unwrap();
        assert!(suggestion.contains(QHY_ALL_IN_ONE_URL), "{suggestion}");
        assert!(
            suggestion.contains(r"C:\Program Files\QHYCCD — present"),
            "{suggestion}"
        );
        assert!(
            suggestion.contains(r"C:\Program Files (x86)\QHYCCD — not found"),
            "{suggestion}"
        );
    }

    #[test]
    fn assemble_orders_config_then_sdk_then_extras() {
        let dir = tempfile::tempdir().unwrap();
        let config_path = dir.path().join("qhy-camera.json");
        let probe = Probe {
            sdk: Some(SdkOutcome::Devices(vec!["QHY178M-1".to_string()])),
            cameras: None,
            extras: vec![Check::ok("hardware.sdk-dll", None, "found")],
        };
        let loaded = load_effective_config(&config_path, &CliOverrides::default());
        let checks = assemble(&config_path, &loaded, probe);
        let names: Vec<&str> = checks.iter().map(|c| c.name.as_str()).collect();
        assert_eq!(
            names,
            [
                "config.full-shape",
                "hardware.sdk-devices",
                "hardware.sdk-dll"
            ]
        );
        assert!(checks.iter().all(|c| c.status == Status::Ok), "{checks:?}");
    }

    #[test]
    fn assemble_skips_the_sdk_check_when_the_dll_never_resolved() {
        let dir = tempfile::tempdir().unwrap();
        let config_path = dir.path().join("qhy-camera.json");
        let probe = Probe {
            sdk: None,
            cameras: None,
            extras: vec![dll_missing_check(&[], &[], &[])],
        };
        let loaded = load_effective_config(&config_path, &CliOverrides::default());
        let names: Vec<String> = assemble(&config_path, &loaded, probe)
            .into_iter()
            .map(|c| c.name)
            .collect();
        assert_eq!(names, ["config.full-shape", "hardware.sdk-dll"]);
    }

    #[test]
    fn operator_attention_tracks_the_windows_installation_checks_only() {
        let ok = || Check::ok("hardware.sdk-dll", None, "found");
        let skew = Check::warn("hardware.sdk-version", None, "differs", None);
        let devices_fail = Check::fail("hardware.sdk-devices", None, "SDK init failed", None);
        assert!(!needs_operator_attention(&[ok()]));
        assert!(needs_operator_attention(&[ok(), skew]));
        assert!(needs_operator_attention(&[dll_missing_check(
            &[],
            &[],
            &[]
        )]));
        assert!(
            !needs_operator_attention(&[ok(), devices_fail]),
            "a failed enumeration is not a Windows installation problem"
        );
    }

    #[test]
    fn prompt_accepts_explicit_yes_variants() {
        for yes in ["y\n", "Y\n", "yes\n", "Yes\n", "YES\n"] {
            let mut input = Cursor::new(yes.as_bytes().to_vec());
            let mut output = Vec::new();
            assert!(
                prompt_open_download_page(&mut input, &mut output),
                "{yes:?} should count as yes"
            );
        }
    }

    #[test]
    fn prompt_defaults_to_no_on_anything_else() {
        for no in ["n\n", "N\n", "\n", "maybe\n", ""] {
            let mut input = Cursor::new(no.as_bytes().to_vec());
            let mut output = Vec::new();
            assert!(
                !prompt_open_download_page(&mut input, &mut output),
                "{no:?} should count as no"
            );
        }
    }

    #[test]
    fn prompt_text_names_the_url() {
        let mut input = Cursor::new(b"n\n".to_vec());
        let mut output = Vec::new();
        prompt_open_download_page(&mut input, &mut output);
        let prompt = String::from_utf8(output).unwrap();
        assert!(prompt.contains(QHY_ALL_IN_ONE_URL), "{prompt}");
        assert!(prompt.contains("[y/N]"), "{prompt}");
    }
}
