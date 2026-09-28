//! Direct ConformU-CLI runner.
//!
//! A feature-gated replacement for `ascom_alpaca::test::ConformUTestBuilder`.
//! The builder is a thin wrapper that runs the external `conformu` binary; the
//! only reason it lived behind `ascom-alpaca`'s `test` feature is that feature's
//! transitive `dtor` dependency — which `crate_universe` (Bazel) resolves only
//! from default features and therefore drops, keeping the conformu integration
//! tests out of the Bazel build entirely. Driving the CLI directly here removes
//! that dependency, so the tests compile under both Cargo and Bazel.
//!
//! The `ConformU` binary is located via the `CONFORMU_PATH` env var (set by the
//! conformu CI workflow and forwarded into the Bazel test sandbox). When it is
//! unset the run is **skipped**, so the conformu integration tests stay inert in
//! the normal cargo/bazel suites and fire only when `ConformU` is explicitly
//! provided — preserving the old `#[ignore]` ergonomics without `#[ignore]`
//! (which Bazel cannot selectively run via a tag).
//!
//! # Two runners, two contracts
//!
//! `ConformU` has two families of commands, and they treat a settings file
//! differently:
//!
//! - The URL-argument commands (`conformance <url>`, `alpacaprotocol <url>`)
//!   read `--settingsfile` and then call `SetFullTest()`, which forces every
//!   setting `ConformU` marks `[MandatoryInFullTest]` to its full-test value
//!   (most of them test selections) and replaces the per-method
//!   `TelescopeTests` dictionary with an all-enabled one. The `conformance`
//!   verb's help text says so (*"with all tests enabled"*); `alpacaprotocol`
//!   does the same without saying so. Everything else in the file survives,
//!   apart from the device address, which comes from the URL: timeouts and
//!   delays, but also tolerances, and settings that shape the test set without
//!   carrying the attribute, among them the `DomeTests` dictionary,
//!   `TestPerformance`, `AlpacaConfiguration.ProtocolTestPrimaryUrlStructure`,
//!   the camera caps (`CameraMaxBinX`, `CameraXMax`, …) and
//!   `SwitchExtendedNumberTestRange`. [`run_conformu`] drives these, and takes
//!   a [`FullRunSettings`] rather than a file so that nothing a caller writes
//!   is silently overridden (a selection) or silently honoured (a tolerance) —
//!   see that type for why it exposes only the timeouts and delays.
//! - The `*-settings` commands read the device **and** the test selection from
//!   the file and honour both. [`run_conformu_from_settings`] drives these; it
//!   is the only entry point where a deselected test takes effect, and a run
//!   made through it carries `ConformU` configuration alerts — which is why it
//!   can never be a `docs/validation/` record.

use std::path::{Path, PathBuf};
use std::process::Stdio;

use tempfile::TempDir;
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::process::Command;

use crate::scratch;

/// Outcome of [`run_conformu`].
#[derive(Debug, PartialEq, Eq)]
pub enum ConformuRun {
    /// `CONFORMU_PATH` was not set, so `ConformU` was not run. Callers treat this
    /// as a pass: the suite is inert unless `ConformU` is explicitly provided.
    Skipped,
    /// `ConformU` ran and reported success (zero exit status).
    Passed,
}

/// The settings a full `ConformU` run honours.
///
/// This is the settings file [`run_conformu`] hands to `ConformU`, as a type:
/// every field is a setting that `ConformU` 4.5.0 reads on the URL-verb path
/// and does not force in `SetFullTest()`. Three kinds of setting are
/// deliberately **absent**:
///
/// - **Test selection**. The URL verbs force every `[MandatoryInFullTest]`
///   flag (`TestSideOfPierWrite`, `TelescopeExtendedPulseGuideTests`,
///   `SwitchEnableSet`, …) and rebuild the `TelescopeTests` dictionary, so a
///   value written here for any of them would document a narrowing that never
///   happens. Settings that shape the test set but that `SetFullTest()`
///   leaves alone (among them the `DomeTests` dictionary, `TestPerformance`,
///   `AlpacaConfiguration.ProtocolTestPrimaryUrlStructure`, the camera caps
///   `CameraMaxBinX` / `CameraXMax` / … and `SwitchExtendedNumberTestRange`)
///   are kept out of this type on purpose, so a run driven here is
///   `ConformU`'s full set for every device class at `ConformU`'s own
///   defaults (every dome test on, no camera cap, the opt-in performance and
///   primary-URL-structure checks off). A device that genuinely cannot run a
///   test uses [`run_conformu_from_settings`].
/// - **Tolerances** (`TelescopePulseGuideTolerance`, `TelescopeSlewTolerance`,
///   …). These *are* honoured, and a loosened one softens the verdict without
///   producing a configuration alert or any trace in the results file — a run
///   made with one could still satisfy the all-zero record rule. Keeping them
///   out of the type keeps every in-tree run on `ConformU`'s own tolerances.
/// - **Application settings** (`ConnectionTimeout`, which is the GUI host's
///   browser-disconnect retention period, `UpdateCheck`, `ApplicationPort`,
///   …). The CLI never reads most of them, so a field for one would promise
///   an effect the run does not have; the exception, `RunAs32Bit`, would make
///   a Windows run exit 0 at once while a detached 32-bit copy of `ConformU`
///   tests the device unobserved.
///
/// Every default equals `ConformU`'s own, and [`run_conformu`] always writes
/// the file — `None` means these defaults — so `ConformU`'s persisted
/// `conform.settings` (the GUI's, under the local application-data folder,
/// tolerances included) is never consulted by an in-tree run. Adding a field
/// means checking, against the `ConformU` source for the version the nightly
/// installs, that the setting is read on the `conformance` / `alpacaprotocol`
/// path, carries no `[MandatoryInFullTest]` attribute, cannot loosen a
/// verdict, and does not change which tests run.
#[derive(Debug, Clone)]
pub struct FullRunSettings {
    /// `ConnectDisconnectTimeout`: seconds `ConformU` waits for `Connecting` to
    /// clear after it calls `Connect()` / `Disconnect()`. `ConformU` default 5.
    pub connect_disconnect_timeout_s: u32,
    /// `FocuserTimeout`: seconds a focuser move may take before the test
    /// fails. `ConformU` default 60.
    pub focuser_timeout_s: u32,
    /// `RotatorTimeout`: seconds a rotator move may take before the test
    /// fails. `ConformU` default 60.
    pub rotator_timeout_s: u32,
    /// `SwitchReadDelay`: milliseconds `ConformU` waits after each switch
    /// read. `ConformU` default 500.
    pub switch_read_delay_ms: u32,
    /// `SwitchWriteDelay`: milliseconds `ConformU` waits after each switch
    /// write. `ConformU` default 3000.
    pub switch_write_delay_ms: u32,
}

impl Default for FullRunSettings {
    fn default() -> Self {
        Self {
            connect_disconnect_timeout_s: 5,
            focuser_timeout_s: 60,
            rotator_timeout_s: 60,
            switch_read_delay_ms: 500,
            switch_write_delay_ms: 3000,
        }
    }
}

impl FullRunSettings {
    /// The settings-file schema version this struct writes. `ConformU` looks
    /// for the literal text `"SettingsCompatibilityVersion":` (no whitespace
    /// before the colon); a file without it is treated as a pre-release file,
    /// renamed aside and replaced by defaults. With it present, every property
    /// the file omits simply keeps its default.
    const SETTINGS_COMPATIBILITY_VERSION: u32 = 1;

    /// The settings file, under `ConformU`'s own property names.
    fn to_json(&self) -> serde_json::Value {
        serde_json::json!({
            "SettingsCompatibilityVersion": Self::SETTINGS_COMPATIBILITY_VERSION,
            "ConnectDisconnectTimeout": self.connect_disconnect_timeout_s,
            "FocuserTimeout": self.focuser_timeout_s,
            "RotatorTimeout": self.rotator_timeout_s,
            "SwitchReadDelay": self.switch_read_delay_ms,
            "SwitchWriteDelay": self.switch_write_delay_ms,
        })
    }

    /// Write the settings file into a fresh scratch directory. The directory
    /// guard is returned with the path: the file exists for exactly as long as
    /// the caller holds it.
    fn write_to_scratch(
        &self,
    ) -> Result<(TempDir, PathBuf), Box<dyn std::error::Error + Send + Sync>> {
        let dir = scratch::new_dir("conformu-settings-")?;
        let path = dir.path().join("conformu-settings.json");
        let json = serde_json::to_string_pretty(&self.to_json())?;
        // The test log used to show the settings literal in the test source;
        // keep what ConformU actually received visible in the run output.
        println!("[conformu settings] {json}");
        std::fs::write(&path, json)?;
        Ok((dir, path))
    }
}

/// How [`run_mode`] treats a non-zero exit whose summary shows zero errors and
/// zero issues — the signature of a run narrowed by deselected tests.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ConfigurationAlerts {
    /// Fail the run. Nothing a [`FullRunSettings`] can write is a selection, so
    /// a run driven through it cannot be narrowed: alerts are not expected and
    /// a non-zero exit is a real verdict.
    Reject,
    /// Accept the run. The `*-settings` verbs honour deselection, and every
    /// deliberately omitted test produces an alert that counts into the exit
    /// code exactly like an error or issue.
    Accept,
}

/// Run both ASCOM `ConformU` suites — `alpacaprotocol`, then `conformance` —
/// against a running Alpaca device.
///
/// Equivalent to:
///
/// ```text
/// conformu alpacaprotocol --settingsfile <generated> <base_url>/api/v1/<device_type>/<device_number>
/// conformu conformance    --settingsfile <generated> <base_url>/api/v1/<device_type>/<device_number>
/// ```
///
/// `device_type` is the lowercase Alpaca device-type URL segment (`"focuser"`,
/// `"camera"`, `"switch"`, `"telescope"`, `"rotator"`, `"covercalibrator"`,
/// `"observingconditions"`, `"safetymonitor"`). `base_url` is the device server
/// root (e.g. `http://127.0.0.1:PORT/`), typically `ServiceHandle::base_url`.
///
/// `settings` shapes the timeouts and delays of the run; `None` runs on
/// `ConformU`'s defaults. A settings file is written either way, so the run
/// never falls back to `ConformU`'s persisted `conform.settings`. The test set
/// is always `ConformU`'s full one — the URL-argument commands call
/// `SetFullTest()` before running — so there is no way to deselect a test
/// through this function; see [`FullRunSettings`] and
/// [`run_conformu_from_settings`].
///
/// Returns [`ConformuRun::Skipped`] when `CONFORMU_PATH` is unset and
/// [`ConformuRun::Passed`] once both suites have exited zero.
///
/// # Errors
///
/// Returns an error if `ConformU` cannot be spawned, the settings file cannot
/// be written, its output cannot be read, or either suite exits non-zero. A
/// full run has nothing to deselect, so no configuration-alert allowance
/// applies here: any non-zero exit is a verdict.
pub async fn run_conformu(
    device_type: &str,
    base_url: &str,
    device_number: u32,
    settings: Option<&FullRunSettings>,
) -> Result<ConformuRun, Box<dyn std::error::Error + Send + Sync>> {
    let Some(conformu) = std::env::var_os("CONFORMU_PATH").filter(|v| !v.is_empty()) else {
        eprintln!("CONFORMU_PATH not set; skipping ConformU run for {device_type}/{device_number}");
        return Ok(ConformuRun::Skipped);
    };

    let device_url = format!(
        "{base}/api/v1/{device_type}/{device_number}",
        base = base_url.trim_end_matches('/'),
    );

    // Always hand ConformU a file: without `--settingsfile` it reads the
    // per-user `conform.settings` the GUI saves into, whose timeouts and
    // tolerances the URL verbs honour. The scratch guard lives across both
    // suites; the file goes with it.
    let settings = settings.cloned().unwrap_or_default();
    let (_settings_dir, settings_path) = settings.write_to_scratch()?;

    // Run both ConformU suites against the device, matching the upstream
    // ascom_alpaca::test runner (`ConformUTestBuilder::run`): `alpacaprotocol`
    // (Alpaca wire-protocol conformance) then `conformance` (full ASCOM
    // device-interface tests). Both must pass.
    for mode in ["alpacaprotocol", "conformance"] {
        run_mode(
            &conformu,
            mode,
            Some(&settings_path),
            Some(&device_url),
            ConfigurationAlerts::Reject,
        )
        .await?;
    }
    Ok(ConformuRun::Passed)
}

/// Run both `ConformU` suites in their `*-settings` variants.
///
/// The device under test **and** the enabled test set both come from
/// `settings_file`: its `AlpacaDevice` block names the device, and
/// `TelescopeTests` etc. select the tests.
///
/// This exists because the URL-argument commands (`alpacaprotocol <url>`,
/// used by [`run_conformu`]) call `ConformU`'s `SetFullTest()`, which forces
/// the full test selection — every `[MandatoryInFullTest]` flag and an
/// all-enabled `TelescopeTests` dictionary — and some capability sets cannot
/// satisfy the full set. The worked example is a `CanPulseGuide = false`
/// Telescope (planetarium-bridge): the protocol suite's `PulseGuide` test
/// polls `IsPulseGuiding` as its completion check and records the
/// spec-mandated `NOT_IMPLEMENTED` answer as an error, so the test must be
/// deselected — which only the `*-settings` commands honor. This is the only
/// entry point where a deselection takes effect, and it is for a documented
/// capability gap, not for a test the device implements and currently fails.
///
/// The file is the caller's to write in full. `ConformU` uses a
/// `TelescopeTests` dictionary exactly as deserialised — a missing key is a
/// `KeyNotFoundException` when the methods phase starts — so a Telescope
/// settings file must spell out every entry (planetarium-bridge's test carries
/// the complete list).
///
/// A deliberately omitted test produces a `ConformU` "configuration alert",
/// and alerts count into the exit code exactly like errors and issues. A
/// run whose only marks are configuration alerts is therefore accepted as a
/// pass here, detected via the summary line `ConformU` prints; errors and
/// issues still fail. Because of those alerts a run made this way never meets
/// the `docs/validation/` record rule.
///
/// # Errors
///
/// Returns an error if `ConformU` cannot be spawned, its output cannot be
/// read, or either `*-settings` suite exits non-zero with errors or issues
/// in its summary.
pub async fn run_conformu_from_settings(
    settings_file: &Path,
) -> Result<ConformuRun, Box<dyn std::error::Error + Send + Sync>> {
    let Some(conformu) = std::env::var_os("CONFORMU_PATH").filter(|v| !v.is_empty()) else {
        eprintln!(
            "CONFORMU_PATH not set; skipping ConformU run for {}",
            settings_file.display()
        );
        return Ok(ConformuRun::Skipped);
    };

    for mode in ["alpacaprotocol-settings", "conformance-settings"] {
        run_mode(
            &conformu,
            mode,
            Some(settings_file),
            None,
            ConfigurationAlerts::Accept,
        )
        .await?;
    }
    Ok(ConformuRun::Passed)
}

/// Run a single `ConformU` mode, streaming its output. `device_url` is the
/// positional device argument for the URL-based commands and `None` for the
/// `*-settings` commands (which read the device from the settings file).
/// Returns `Err` on a non-zero exit, except — under
/// [`ConfigurationAlerts::Accept`] — when the output's summary line shows
/// zero errors and zero issues: the exit code also counts configuration
/// alerts (deliberately deselected tests), which are not device defects.
async fn run_mode(
    conformu: &std::ffi::OsStr,
    mode: &str,
    settings_file: Option<&Path>,
    device_url: Option<&str>,
    alerts: ConfigurationAlerts,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let mut command = Command::new(conformu);
    command.arg(mode);
    // ConformU writes a per-run log tree under $HOME (e.g.
    // $HOME/Documents/ascom/logs<date>). Under Bazel's test sandbox the real
    // $HOME is read-only, so ConformU aborts on startup; point HOME at the
    // test's writable TEST_TMPDIR. (Under Cargo there is no sandbox and $HOME is
    // already writable, so this is a no-op there.)
    if let Some(tmp) = std::env::var_os("TEST_TMPDIR") {
        command.env("HOME", tmp);
    }
    // For the URL-based commands the file carries the timeouts and delays of
    // a `FullRunSettings` (test selection is not expressible there — the
    // command calls SetFullTest()); for the `*-settings` commands it carries
    // the device and the test selection too.
    if let Some(path) = settings_file {
        command.arg("--settingsfile").arg(path);
    }
    if let Some(url) = device_url {
        command.arg(url);
    }
    let mut child = command.stdout(Stdio::piped()).spawn()?;

    // Stream ConformU's (unstructured) stdout into the test log so progress is
    // visible and a verbose run can't deadlock on an undrained pipe. The
    // summary lines are also inspected for the alerts-only pass below.
    let mut clean_except_alerts = false;
    if let Some(stdout) = child.stdout.take() {
        let mut lines = BufReader::new(stdout).lines();
        while let Some(line) = lines.next_line().await? {
            println!("[conformu {mode}] {line}");
            // The two summary shapes: the conformance suite prints
            // "Your device had 0 issues, 0 errors and N configuration
            // alert(s)"; the protocol suite prints "Found 0 errors, 0
            // issues and N information messages" (informational messages
            // never affect its exit code).
            if line.contains("0 issues, 0 errors and") {
                clean_except_alerts = true;
            }
        }
    }

    let status = child.wait().await?;
    let target = device_url.unwrap_or("the settings-file device");
    if status.success() {
        return Ok(());
    }
    match (alerts, clean_except_alerts) {
        (ConfigurationAlerts::Accept, true) => {
            println!(
                "[conformu {mode}] non-zero exit {status} accepted: the summary reported 0 issues \
                 and 0 errors (configuration alerts only)"
            );
            Ok(())
        }
        (ConfigurationAlerts::Reject, true) => Err(format!(
            "ConformU `{mode}` exited with {status} testing {target} although its summary \
             reported 0 issues and 0 errors: the run was narrowed by configuration alerts, \
             which a full run never has — a deselected test only takes effect through \
             run_conformu_from_settings"
        )
        .into()),
        (_, false) => {
            Err(format!("ConformU `{mode}` exited with {status} testing {target}").into())
        }
    }
}

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
mod tests {
    use super::FullRunSettings;

    /// Every `Settings` property `ConformU` 4.5.0 marks `[MandatoryInFullTest]`
    /// — forced by `SetFullTest()` on the URL-verb path — plus the
    /// `TelescopeTests` dictionary it rebuilds. None may ever appear in the
    /// file [`super::run_conformu`] writes: a value there would be overridden
    /// silently, which is exactly the drift this type exists to prevent.
    const FORCED_BY_SET_FULL_TEST: &[&str] = &[
        "AllowConnectedTrueAfterDisconnect",
        "DisplayMethodCalls",
        "Debug",
        "TraceDiscovery",
        "TraceAlpacaCalls",
        "TestProperties",
        "TestMethods",
        "TelescopeExtendedRateOffsetTests",
        "TelescopeFirstUseTests",
        "TestSideOfPierRead",
        "TestSideOfPierWrite",
        "TelescopeExtendedPulseGuideTests",
        "TelescopeExtendedMoveAxisTests",
        "TelescopeExtendedSiteTests",
        "TelescopeTests",
        "CameraFirstUseTests",
        "CameraTestImageArrayVariant",
        "DomeOpenShutter",
        "SwitchEnableSet",
        "SwitchTestOffsets",
    ];

    /// Settings the URL verbs honour that would soften a verdict without a
    /// configuration alert. Kept out of the type on purpose.
    const VERDICT_SOFTENERS: &[&str] = &[
        "TelescopePulseGuideTolerance",
        "TelescopeSlewTolerance",
        "TelescopeMaximumSlewTime",
        "DomeSlewTolerance",
        "FocuserMoveTolerance",
    ];

    /// Application settings no field may carry. Most are read only on
    /// `ConformU`'s GUI paths, so a field for one would promise an effect a
    /// CLI run does not have. `RunAs32Bit` is read by the CLI too: on 64-bit
    /// Windows it makes `ConformU` start a detached 32-bit copy of itself on
    /// the same command line and exit 0 at once, so the caller sees a pass
    /// while that copy, whose verdict nobody reads, drives the device.
    const APPLICATION_ONLY: &[&str] = &[
        "ConnectionTimeout",
        "GoHomeOnDeviceSelected",
        "RunAs32Bit",
        "RiskAcknowledged",
        "ApplicationPort",
        "UpdateCheck",
    ];

    fn keys(value: &serde_json::Value) -> Vec<String> {
        let mut keys: Vec<String> = value
            .as_object()
            .expect("the settings file is a JSON object")
            .keys()
            .cloned()
            .collect();
        keys.sort();
        keys
    }

    #[test]
    fn default_file_carries_the_compatibility_version_and_only_honoured_keys() {
        let json = FullRunSettings::default().to_json();

        let mut expected = vec![
            "ConnectDisconnectTimeout",
            "FocuserTimeout",
            "RotatorTimeout",
            "SettingsCompatibilityVersion",
            "SwitchReadDelay",
            "SwitchWriteDelay",
        ];
        expected.sort_unstable();
        assert_eq!(keys(&json), expected);
        assert_eq!(json["SettingsCompatibilityVersion"], 1);
    }

    #[test]
    fn defaults_equal_conformu_defaults() {
        let json = FullRunSettings::default().to_json();

        assert_eq!(json["ConnectDisconnectTimeout"], 5);
        assert_eq!(json["FocuserTimeout"], 60);
        assert_eq!(json["RotatorTimeout"], 60);
        assert_eq!(json["SwitchReadDelay"], 500);
        assert_eq!(json["SwitchWriteDelay"], 3000);
    }

    #[test]
    fn no_forced_softening_or_application_only_key_is_ever_written() {
        let json = FullRunSettings::default().to_json();
        let object = json
            .as_object()
            .expect("the settings file is a JSON object");

        for key in FORCED_BY_SET_FULL_TEST
            .iter()
            .chain(VERDICT_SOFTENERS)
            .chain(APPLICATION_ONLY)
        {
            assert!(
                !object.contains_key(*key),
                "{key} must not be written: the URL verbs override it, silently honour it, \
                 or never read it"
            );
        }
    }

    #[test]
    fn overrides_land_under_their_conformu_names() {
        let json = FullRunSettings {
            connect_disconnect_timeout_s: 10,
            focuser_timeout_s: 30,
            rotator_timeout_s: 31,
            switch_read_delay_ms: 50,
            switch_write_delay_ms: 100,
        }
        .to_json();

        assert_eq!(json["ConnectDisconnectTimeout"], 10);
        assert_eq!(json["FocuserTimeout"], 30);
        assert_eq!(json["RotatorTimeout"], 31);
        assert_eq!(json["SwitchReadDelay"], 50);
        assert_eq!(json["SwitchWriteDelay"], 100);
    }

    #[test]
    fn the_written_file_is_the_settings_json_under_the_returned_guard() {
        let settings = FullRunSettings::default();

        let (dir, path) = settings.write_to_scratch().unwrap();

        assert_eq!(path.parent().unwrap(), dir.path());
        assert_eq!(path.file_name().unwrap(), "conformu-settings.json");
        let text = std::fs::read_to_string(&path).unwrap();
        // The literal ConformU's pre-release detector looks for: without it the
        // file is renamed aside and the run silently proceeds on defaults.
        assert!(text.contains("\"SettingsCompatibilityVersion\":"));
        let on_disk: serde_json::Value = serde_json::from_str(&text).unwrap();
        assert_eq!(on_disk, settings.to_json());
    }

    /// The strictness boundary itself, exercised against a stand-in
    /// `conformu`: a shell script that prints a chosen summary line and exits
    /// with a chosen status. Unix-only because the stand-in is a `#!/bin/sh`
    /// script; `run_mode`'s policy logic has no platform arm, so the Windows
    /// leg of the Bazel target simply selects nothing here.
    #[cfg(unix)]
    mod stand_in_conformu {
        use std::ffi::OsString;
        use std::os::unix::fs::PermissionsExt;
        use std::path::{Path, PathBuf};

        use tempfile::TempDir;

        use crate::conformu::{
            run_conformu, run_conformu_from_settings, run_mode, ConfigurationAlerts, ConformuRun,
        };
        use crate::scratch;

        const CLEAN_WITH_ALERTS: &str =
            "Your device had 0 issues, 0 errors and 2 configuration alerts";
        const REAL_ISSUES: &str = "Your device had 3 issues, 0 errors and 0 configuration alerts";
        const URL: &str = "http://127.0.0.1:1/api/v1/telescope/0";

        /// These tests fork. A child forked by one test in the window between
        /// another test writing its script and closing it inherits that
        /// still-open descriptor, and the second test's exec then fails with
        /// `ETXTBSY` ("Text file busy"). Serialising each write-then-spawn
        /// sequence removes the overlap.
        static ONE_AT_A_TIME: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

        /// A `conformu` that appends its arguments to `args.log` beside itself
        /// (one line per invocation), prints `summary` and exits with `code`.
        /// The guard owns the script's directory.
        fn stand_in(summary: &str, code: i32) -> (TempDir, PathBuf) {
            let dir = scratch::new_dir("stand-in-conformu-").unwrap();
            let path = dir.path().join("conformu");
            let log = dir.path().join("args.log");
            let script = format!(
                "#!/bin/sh\nprintf '%s\\n' \"$*\" >> '{log}'\necho '{summary}'\nexit {code}\n",
                log = log.display()
            );
            std::fs::write(&path, script).unwrap();
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
            (dir, path)
        }

        /// The argument lines the stand-in recorded, in call order.
        fn invocations(dir: &TempDir) -> Vec<String> {
            std::fs::read_to_string(dir.path().join("args.log"))
                .unwrap()
                .lines()
                .map(str::to_owned)
                .collect()
        }

        /// Points `CONFORMU_PATH` at `value` (or unsets it) for the guard's
        /// lifetime and restores whatever was there before, panic or not. The
        /// runners read the variable directly, and these tests already take
        /// turns, so the swap is not observed by anyone else.
        struct ConformuPath(Option<OsString>);

        impl ConformuPath {
            fn set(value: Option<&Path>) -> Self {
                let previous = std::env::var_os("CONFORMU_PATH");
                match value {
                    Some(path) => std::env::set_var("CONFORMU_PATH", path),
                    None => std::env::remove_var("CONFORMU_PATH"),
                }
                Self(previous)
            }
        }

        impl Drop for ConformuPath {
            fn drop(&mut self) {
                match &self.0 {
                    Some(previous) => std::env::set_var("CONFORMU_PATH", previous),
                    None => std::env::remove_var("CONFORMU_PATH"),
                }
            }
        }

        #[tokio::test]
        async fn run_conformu_hands_a_settings_file_and_the_device_url_to_both_suites() {
            let _serial = ONE_AT_A_TIME.lock().await;
            let (dir, conformu) = stand_in("Congratulations, no errors", 0);
            let _env = ConformuPath::set(Some(&conformu));

            let outcome = run_conformu("telescope", "http://127.0.0.1:1/", 0, None)
                .await
                .unwrap();

            assert_eq!(outcome, ConformuRun::Passed);
            let calls = invocations(&dir);
            assert_eq!(calls.len(), 2, "{calls:?}");
            for (call, mode) in calls.iter().zip(["alpacaprotocol", "conformance"]) {
                assert!(
                    call.starts_with(&format!("{mode} --settingsfile ")),
                    "{call}"
                );
                assert!(
                    call.ends_with("/conformu-settings.json http://127.0.0.1:1/api/v1/telescope/0"),
                    "{call}"
                );
            }
        }

        #[tokio::test]
        async fn run_conformu_from_settings_runs_both_settings_suites_on_the_given_file() {
            let _serial = ONE_AT_A_TIME.lock().await;
            let (dir, conformu) = stand_in(
                "Your device had 0 issues, 0 errors and 1 configuration alert",
                1,
            );
            let _env = ConformuPath::set(Some(&conformu));

            let outcome = run_conformu_from_settings(Path::new("/settings/bridge.json"))
                .await
                .unwrap();

            assert_eq!(outcome, ConformuRun::Passed);
            assert_eq!(
                invocations(&dir),
                [
                    "alpacaprotocol-settings --settingsfile /settings/bridge.json",
                    "conformance-settings --settingsfile /settings/bridge.json",
                ]
            );
        }

        #[tokio::test]
        async fn both_runners_skip_when_conformu_path_is_unset_or_empty() {
            let _serial = ONE_AT_A_TIME.lock().await;

            for value in [None, Some(Path::new(""))] {
                let _env = ConformuPath::set(value);
                let url_run = run_conformu("focuser", "http://127.0.0.1:1", 0, None)
                    .await
                    .unwrap();
                let settings_run = run_conformu_from_settings(Path::new("/nowhere.json"))
                    .await
                    .unwrap();
                assert_eq!(
                    (url_run, settings_run),
                    (ConformuRun::Skipped, ConformuRun::Skipped)
                );
            }
        }

        #[tokio::test]
        async fn a_full_run_rejects_a_clean_summary_with_a_non_zero_exit() {
            let _serial = ONE_AT_A_TIME.lock().await;
            let (_dir, conformu) = stand_in(CLEAN_WITH_ALERTS, 2);

            let err = run_mode(
                conformu.as_os_str(),
                "conformance",
                None,
                Some(URL),
                ConfigurationAlerts::Reject,
            )
            .await
            .unwrap_err();

            assert!(
                err.to_string().contains("narrowed by configuration alerts"),
                "unexpected error text: {err}"
            );
        }

        #[tokio::test]
        async fn a_settings_run_accepts_a_clean_summary_with_a_non_zero_exit() {
            let _serial = ONE_AT_A_TIME.lock().await;
            let (_dir, conformu) = stand_in(CLEAN_WITH_ALERTS, 2);

            run_mode(
                conformu.as_os_str(),
                "conformance-settings",
                None,
                None,
                ConfigurationAlerts::Accept,
            )
            .await
            .unwrap();
        }

        #[tokio::test]
        async fn a_non_zero_exit_with_real_issues_fails_under_both_policies() {
            let _serial = ONE_AT_A_TIME.lock().await;
            for policy in [ConfigurationAlerts::Reject, ConfigurationAlerts::Accept] {
                let (_dir, conformu) = stand_in(REAL_ISSUES, 3);

                let err = run_mode(conformu.as_os_str(), "conformance", None, Some(URL), policy)
                    .await
                    .unwrap_err();

                assert!(
                    err.to_string().contains("exited with exit status: 3"),
                    "{policy:?}: unexpected error text: {err}"
                );
            }
        }

        #[tokio::test]
        async fn a_zero_exit_passes_under_both_policies() {
            let _serial = ONE_AT_A_TIME.lock().await;
            for policy in [ConfigurationAlerts::Reject, ConfigurationAlerts::Accept] {
                let (_dir, conformu) = stand_in("Congratulations, no errors", 0);

                run_mode(conformu.as_os_str(), "conformance", None, Some(URL), policy)
                    .await
                    .unwrap();
            }
        }
    }
}
