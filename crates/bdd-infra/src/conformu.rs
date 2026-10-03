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
//!   can never be a `docs/validation/` record. The caller names the alerts it
//!   expects, and the run fails on any other set.
//!
//! Neither runner parses `ConformU`'s console output. Both read the results
//! file the conformance suite writes (`--resultsfile`): a full run passes only
//! with a zero exit **and** a results file free of errors, issues and alerts;
//! a settings run passes on that file alone, with no error, no issue and
//! exactly the expected alerts (the alerts make its exit status non-zero).
//! The file is the verdict rather than the exit status because `ConformU`
//! exits with the count of errors, issues and alerts, which Unix truncates to
//! its low eight bits — 256 findings read as success.

use std::path::{Path, PathBuf};
use std::process::{ExitStatus, Stdio};

use tempfile::TempDir;
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::process::Command;

use crate::scratch;

/// Outcome of [`run_conformu`] and [`run_conformu_from_settings`].
#[derive(Debug, PartialEq, Eq)]
pub enum ConformuRun {
    /// `CONFORMU_PATH` was not set, so `ConformU` was not run. Callers treat this
    /// as a pass: the suite is inert unless `ConformU` is explicitly provided.
    Skipped,
    /// `ConformU` ran and both suites met the runner's verdict: the protocol
    /// suite exited zero, and the conformance suite's results file lists no
    /// error, no issue and — for [`run_conformu`], whose conformance suite
    /// must also exit zero — no configuration alert, or for
    /// [`run_conformu_from_settings`] exactly the expected ones.
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

/// The keys of `ConformU` 4.5.0's default `Settings.TelescopeTests` dictionary.
///
/// A settings file that carries `TelescopeTests` replaces that default
/// outright: `ConformU` uses the dictionary exactly as deserialised and indexes
/// it by these names — the protocol suite fourteen of them, the conformance
/// suite's methods phase all seventeen — so a missing key throws a
/// `KeyNotFoundException` wherever a suite first reaches it, abandoning the
/// rest of that suite as an error or issue that names only that one key. The
/// list is pinned to the version the nightly installs today. A newer
/// `ConformU` that adds a key still fails a file written to this list —
/// loudly, the same way — and the key then belongs here.
const TELESCOPE_TESTS: [&str; 17] = [
    "CanMoveAxis",
    "Park/Unpark",
    "AbortSlew",
    "AxisRate",
    "FindHome",
    "MoveAxis",
    "PulseGuide",
    "SlewToCoordinates",
    "SlewToCoordinatesAsync",
    "SlewToTarget",
    "SlewToTargetAsync",
    "DestinationSideOfPier",
    "SlewToAltAz",
    "SlewToAltAzAsync",
    "SyncToCoordinates",
    "SyncToTarget",
    "SyncToAltAz",
];

/// Refuse a settings file whose `TelescopeTests` dictionary would abandon a
/// suite part-way, naming every missing key at once. A file without the dictionary
/// keeps `ConformU`'s all-enabled default and passes.
fn check_telescope_tests(settings: &serde_json::Value) -> Result<(), String> {
    let Some(tests) = settings.get("TelescopeTests") else {
        return Ok(());
    };
    let tests = tests
        .as_object()
        .ok_or_else(|| format!("`TelescopeTests` is not a JSON object: {tests}"))?;
    let missing: Vec<&str> = TELESCOPE_TESTS
        .into_iter()
        .filter(|key| !tests.contains_key(*key))
        .collect();
    if missing.is_empty() {
        Ok(())
    } else {
        Err(format!(
            "`TelescopeTests` lacks {missing:?}: ConformU indexes the dictionary as written, \
             so a missing key abandons the run part-way — spell out every key, `false` for a \
             deselected test"
        ))
    }
}

/// The verdict-bearing part of the results file `ConformU`'s conformance suite
/// writes when given `--resultsfile` (its `ConformResults` class). Every entry
/// is the `Key` / `Value` pair `ConformU` records: where the finding arose (a
/// test, a stage, or `Conform configuration` for every alert), then the
/// message — which is why alerts are compared by message alone.
#[derive(Debug, Default, PartialEq, Eq)]
struct ConformResults {
    errors: Vec<(String, String)>,
    issues: Vec<(String, String)>,
    configuration_alerts: Vec<(String, String)>,
}

impl ConformResults {
    fn parse(json: &str) -> Result<Self, String> {
        let value: serde_json::Value =
            serde_json::from_str(json).map_err(|e| format!("not JSON: {e}"))?;
        let entries = |field: &str| -> Result<Vec<(String, String)>, String> {
            value
                .get(field)
                .and_then(serde_json::Value::as_array)
                .ok_or_else(|| format!("no `{field}` array"))?
                .iter()
                .map(|entry| {
                    let text = |name: &str| {
                        entry
                            .get(name)
                            .and_then(serde_json::Value::as_str)
                            .map(str::to_owned)
                    };
                    text("Key").zip(text("Value")).ok_or_else(|| {
                        format!("a `{field}` entry is not a Key/Value pair: {entry}")
                    })
                })
                .collect()
        };
        Ok(Self {
            errors: entries("Errors")?,
            issues: entries("Issues")?,
            configuration_alerts: entries("ConfigurationAlerts")?,
        })
    }

    fn read(path: &Path) -> Result<Self, String> {
        let json = std::fs::read_to_string(path)
            .map_err(|e| format!("cannot read {}: {e}", path.display()))?;
        Self::parse(&json).map_err(|e| format!("{}: {e}", path.display()))
    }

    /// The configuration alerts' messages, sorted, so two lists compare equal
    /// whatever their order — duplicates still count.
    fn alert_messages(&self) -> Vec<&str> {
        let mut messages: Vec<&str> = self
            .configuration_alerts
            .iter()
            .map(|(_, message)| message.as_str())
            .collect();
        messages.sort_unstable();
        messages
    }

    fn describe_defects(&self) -> String {
        format!(
            "{} issue(s) {:?} and {} error(s) {:?}",
            self.issues.len(),
            self.issues,
            self.errors.len(),
            self.errors
        )
    }
}

/// A full run's conformance verdict: it passes with a zero exit and a results
/// file that lists nothing — no error, no issue and no configuration alert. A
/// full run has nothing to deselect, so a non-zero exit, any listed finding,
/// or no readable results file fails it. `status` is the exit status as
/// `ConformU`'s process reported it, for the message.
fn full_run_verdict(
    target: &str,
    status: &str,
    exited_zero: bool,
    results: Result<ConformResults, String>,
) -> Result<(), String> {
    let results = results.map_err(|e| {
        format!(
            "ConformU `conformance` exited with {status} testing {target} and left no \
             readable results file ({e})"
        )
    })?;
    if !results.issues.is_empty() || !results.errors.is_empty() {
        return Err(format!(
            "ConformU `conformance` exited with {status} testing {target}: {}",
            results.describe_defects()
        ));
    }
    if !results.configuration_alerts.is_empty() {
        return Err(format!(
            "ConformU `conformance` exited with {status} testing {target} with 0 issues and \
             0 errors: the run was narrowed by configuration alerts {:?}, which a full run never \
             has — a deselected test only takes effect through run_conformu_from_settings",
            results.alert_messages()
        ));
    }
    if exited_zero {
        Ok(())
    } else {
        Err(format!(
            "ConformU `conformance` exited with {status} testing {target} although its results \
             file lists no error, issue or alert"
        ))
    }
}

/// A settings run passes with no error, no issue and exactly the expected
/// configuration alerts — compared as lists of messages in any order, so a
/// missing, extra, duplicated or reworded alert fails it.
fn settings_run_verdict(results: &ConformResults, expected_alerts: &[&str]) -> Result<(), String> {
    if !results.issues.is_empty() || !results.errors.is_empty() {
        return Err(format!(
            "ConformU `conformance-settings` reported {}",
            results.describe_defects()
        ));
    }
    let raised = results.alert_messages();
    let mut expected = expected_alerts.to_vec();
    expected.sort_unstable();
    if raised == expected {
        Ok(())
    } else {
        Err(format!(
            "ConformU `conformance-settings` raised configuration alerts {raised:?}, but the \
             caller expects exactly {expected:?}: every deselection is a documented decision, \
             so a change to the selection changes the expected list (and the design doc) too"
        ))
    }
}

/// Run both ASCOM `ConformU` suites — `alpacaprotocol`, then `conformance` —
/// against a running Alpaca device.
///
/// Equivalent to:
///
/// ```text
/// conformu alpacaprotocol --settingsfile <generated> <base_url>/api/v1/<device_type>/<device_number>
/// conformu conformance    --settingsfile <generated> --resultsfile <scratch> <base_url>/api/v1/<device_type>/<device_number>
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
/// [`ConformuRun::Passed`] once both suites have exited zero and the
/// conformance suite's results file lists no error, issue or configuration
/// alert.
///
/// # Errors
///
/// Returns an error if `ConformU` cannot be spawned, the settings file cannot
/// be written, its output cannot be read, either suite exits non-zero, or the
/// conformance suite leaves no readable results file or one that lists any
/// finding. A full run has nothing to deselect, so no configuration-alert
/// allowance applies here.
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
    // suites; the file, and the results file beside it, go with it.
    let settings = settings.cloned().unwrap_or_default();
    let (settings_dir, settings_path) = settings.write_to_scratch()?;
    let results_path = settings_dir.path().join("conformance-results.json");

    // Run both ConformU suites against the device, matching the upstream
    // ascom_alpaca::test runner (`ConformUTestBuilder::run`): `alpacaprotocol`
    // (Alpaca wire-protocol conformance) then `conformance` (full ASCOM
    // device-interface tests). Both must pass.
    let status = run_mode(
        &conformu,
        "alpacaprotocol",
        &settings_path,
        None,
        Some(&device_url),
    )
    .await?;
    if !status.success() {
        return Err(
            format!("ConformU `alpacaprotocol` exited with {status} testing {device_url}").into(),
        );
    }
    let status = run_mode(
        &conformu,
        "conformance",
        &settings_path,
        Some(&results_path),
        Some(&device_url),
    )
    .await?;
    full_run_verdict(
        &device_url,
        &status.to_string(),
        status.success(),
        ConformResults::read(&results_path),
    )?;
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
/// `KeyNotFoundException` wherever a suite first reaches it — so a Telescope
/// settings file that carries the dictionary must spell out every entry
/// (planetarium-bridge's test carries the complete list). The file is checked
/// for that before `ConformU` starts, and the error names each missing key.
///
/// A deliberately omitted test produces a `ConformU` "configuration alert",
/// and alerts count into the conformance suite's exit code exactly like errors
/// and issues. That suite's verdict is therefore read from the results file
/// it writes, not from its exit code: the run passes with no error, no issue
/// and exactly the configuration alerts named in `expected_alerts` — each one
/// the alert's message as `ConformU` words it (e.g. `"Pulse guide tests were
/// omitted due to Conform configuration."`), in any order. A missing or extra
/// alert fails, so the file cannot deselect a test that raises an alert without
/// its caller documenting it. Settings that narrow or soften a run without
/// any alert — the camera caps, `SwitchExtendedNumberTestRange`, the
/// tolerances — are honoured silently and are not caught here; a settings
/// file leaves them at `ConformU`'s defaults. The protocol suite reports a
/// deselected test as an information message that never touches its exit
/// code, so it must exit zero. Because of the alerts a run made this way never
/// meets the `docs/validation/` record rule.
///
/// # Errors
///
/// Returns an error if the settings file cannot be read, is not JSON or
/// carries an incomplete `TelescopeTests` dictionary; if `ConformU` cannot be
/// spawned or its output cannot be read; if the protocol suite exits
/// non-zero; or if the conformance suite leaves no readable results file, or
/// one with an error, an issue, or a configuration-alert set other than
/// `expected_alerts`.
pub async fn run_conformu_from_settings(
    settings_file: &Path,
    expected_alerts: &[&str],
) -> Result<ConformuRun, Box<dyn std::error::Error + Send + Sync>> {
    let Some(conformu) = std::env::var_os("CONFORMU_PATH").filter(|v| !v.is_empty()) else {
        eprintln!(
            "CONFORMU_PATH not set; skipping ConformU run for {}",
            settings_file.display()
        );
        return Ok(ConformuRun::Skipped);
    };

    let settings = std::fs::read_to_string(settings_file)
        .map_err(|e| format!("cannot read settings file {}: {e}", settings_file.display()))?;
    let settings: serde_json::Value = serde_json::from_str(&settings)
        .map_err(|e| format!("settings file {} is not JSON: {e}", settings_file.display()))?;
    check_telescope_tests(&settings)
        .map_err(|e| format!("settings file {}: {e}", settings_file.display()))?;

    let status = run_mode(
        &conformu,
        "alpacaprotocol-settings",
        settings_file,
        None,
        None,
    )
    .await?;
    if !status.success() {
        return Err(format!(
            "ConformU `alpacaprotocol-settings` exited with {status} testing the device in {}",
            settings_file.display()
        )
        .into());
    }

    let results_dir = scratch::new_dir("conformu-results-")?;
    let results_path = results_dir.path().join("conformance-results.json");
    let status = run_mode(
        &conformu,
        "conformance-settings",
        settings_file,
        Some(&results_path),
        None,
    )
    .await?;
    let results = ConformResults::read(&results_path).map_err(|e| {
        format!("ConformU `conformance-settings` exited with {status} and left no readable results file: {e}")
    })?;
    settings_run_verdict(&results, expected_alerts)?;
    println!(
        "[conformu conformance-settings] {status} accepted: 0 issues, 0 errors and exactly the \
         expected configuration alerts {:?}",
        results.alert_messages()
    );
    Ok(ConformuRun::Passed)
}

/// Run a single `ConformU` mode, streaming its output into the test log, and
/// return its exit status for the caller to judge. `results_file` adds
/// `--resultsfile` (the conformance suites); `device_url` is the positional
/// device argument for the URL-based commands and `None` for the `*-settings`
/// commands, which read the device from the settings file.
async fn run_mode(
    conformu: &std::ffi::OsStr,
    mode: &str,
    settings_file: &Path,
    results_file: Option<&Path>,
    device_url: Option<&str>,
) -> Result<ExitStatus, Box<dyn std::error::Error + Send + Sync>> {
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
    command.arg("--settingsfile").arg(settings_file);
    if let Some(path) = results_file {
        command.arg("--resultsfile").arg(path);
    }
    if let Some(url) = device_url {
        command.arg(url);
    }
    let mut child = command.stdout(Stdio::piped()).spawn()?;

    // Stream ConformU's (unstructured) stdout into the test log so progress is
    // visible and a verbose run can't deadlock on an undrained pipe. Nothing
    // here parses it: the verdict comes from the exit status and the results
    // file.
    if let Some(stdout) = child.stdout.take() {
        let mut lines = BufReader::new(stdout).lines();
        while let Some(line) = lines.next_line().await? {
            println!("[conformu {mode}] {line}");
        }
    }

    Ok(child.wait().await?)
}

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
mod tests {
    use super::{
        check_telescope_tests, full_run_verdict, settings_run_verdict, ConformResults,
        FullRunSettings, TELESCOPE_TESTS,
    };

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
    /// the same command line and exit 0 at once, before any results file
    /// exists — so the run fails, while that copy, whose verdict nobody
    /// reads, still drives the device.
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

    /// A `TelescopeTests` dictionary with every key `ConformU` defines, all on.
    fn complete_telescope_tests() -> serde_json::Value {
        TELESCOPE_TESTS
            .into_iter()
            .map(|key| (key.to_owned(), serde_json::Value::Bool(true)))
            .collect::<serde_json::Map<_, _>>()
            .into()
    }

    #[test]
    fn a_complete_telescope_tests_dictionary_passes_the_check() {
        let settings = serde_json::json!({ "TelescopeTests": complete_telescope_tests() });

        check_telescope_tests(&settings).unwrap();
    }

    #[test]
    fn a_settings_file_without_telescope_tests_passes_the_check() {
        let settings = serde_json::json!({ "SettingsCompatibilityVersion": 1 });

        check_telescope_tests(&settings).unwrap();
    }

    #[test]
    fn the_check_names_every_missing_telescope_test() {
        let mut tests = complete_telescope_tests();
        let object = tests.as_object_mut().unwrap();
        object.remove("CanMoveAxis");
        object.remove("Park/Unpark");
        let settings = serde_json::json!({ "TelescopeTests": tests });

        let err = check_telescope_tests(&settings).unwrap_err();

        assert!(
            err.contains(r#"["CanMoveAxis", "Park/Unpark"]"#),
            "unexpected error text: {err}"
        );
    }

    #[test]
    fn the_check_refuses_a_telescope_tests_value_that_is_not_an_object() {
        let settings = serde_json::json!({ "TelescopeTests": [] });

        let err = check_telescope_tests(&settings).unwrap_err();

        assert!(
            err.contains("not a JSON object"),
            "unexpected error text: {err}"
        );
    }

    const PULSE_GUIDE_ALERT: &str = "Pulse guide tests were omitted due to Conform configuration.";
    const SIDE_OF_PIER_READ_ALERT: &str =
        "Extended side of pier read tests were omitted due to Conform configuration.";

    /// `ConformU` 4.5.0's results-file shape — every field it writes, entries
    /// as `Key` / `Value` pairs the way a planetarium-bridge run's file carries
    /// them — with one invented issue so the parser sees a non-empty list.
    const RESULTS_FILE: &str = r#"{
      "ErrorCount": 0,
      "IssueCount": 1,
      "ConfigurationAlertCount": 1,
      "TimingIssuesCount": 0,
      "TimingCount": 62,
      "Errors": [],
      "Issues": [ { "Key": "SlewToTarget", "Value": "Slewed 12.3 arc seconds away" } ],
      "ConfigurationAlerts": [
        { "Key": "Conform configuration", "Value": "Pulse guide tests were omitted due to Conform configuration." }
      ],
      "Timings": [ { "Key": "AbortSlew", "Value": "0.1" } ]
    }"#;

    fn alerts(messages: &[&str]) -> ConformResults {
        ConformResults {
            configuration_alerts: messages
                .iter()
                .map(|m| ("Conform configuration".to_owned(), (*m).to_owned()))
                .collect(),
            ..ConformResults::default()
        }
    }

    #[test]
    fn a_results_file_parses_into_its_three_verdict_lists() {
        let results = ConformResults::parse(RESULTS_FILE).unwrap();

        assert_eq!(
            results,
            ConformResults {
                errors: vec![],
                issues: vec![(
                    "SlewToTarget".to_owned(),
                    "Slewed 12.3 arc seconds away".to_owned()
                )],
                configuration_alerts: vec![(
                    "Conform configuration".to_owned(),
                    PULSE_GUIDE_ALERT.to_owned()
                )],
            }
        );
    }

    #[test]
    fn a_results_file_missing_any_verdict_list_is_refused() {
        for field in ["Errors", "Issues", "ConfigurationAlerts"] {
            let mut file =
                serde_json::json!({ "Errors": [], "Issues": [], "ConfigurationAlerts": [] });
            file.as_object_mut().unwrap().remove(field);

            let err = ConformResults::parse(&file.to_string()).unwrap_err();

            assert!(
                err.contains(&format!("no `{field}` array")),
                "{field}: unexpected error text: {err}"
            );
        }
    }

    #[test]
    fn a_settings_run_passes_with_exactly_the_expected_alerts_in_any_order() {
        // ConformU's emission order, which is not sorted.
        let results = alerts(&[PULSE_GUIDE_ALERT, SIDE_OF_PIER_READ_ALERT]);

        settings_run_verdict(&results, &[PULSE_GUIDE_ALERT, SIDE_OF_PIER_READ_ALERT]).unwrap();
        settings_run_verdict(&results, &[SIDE_OF_PIER_READ_ALERT, PULSE_GUIDE_ALERT]).unwrap();
    }

    #[test]
    fn a_settings_run_with_an_unexpected_alert_fails() {
        let results = alerts(&[PULSE_GUIDE_ALERT, SIDE_OF_PIER_READ_ALERT]);

        let err = settings_run_verdict(&results, &[PULSE_GUIDE_ALERT]).unwrap_err();

        assert!(
            err.contains("expects exactly"),
            "unexpected error text: {err}"
        );
    }

    #[test]
    fn a_settings_run_missing_an_expected_alert_fails() {
        let results = alerts(&[]);

        let err = settings_run_verdict(&results, &[PULSE_GUIDE_ALERT]).unwrap_err();

        assert!(
            err.contains("expects exactly"),
            "unexpected error text: {err}"
        );
    }

    /// `ConformU` can raise one alert several times (a Switch's skipped
    /// offset tests, once per switch), so each expected occurrence counts.
    #[test]
    fn a_settings_run_raising_an_expected_alert_twice_fails() {
        let results = alerts(&[PULSE_GUIDE_ALERT, PULSE_GUIDE_ALERT]);

        let err = settings_run_verdict(&results, &[PULSE_GUIDE_ALERT]).unwrap_err();

        assert!(
            err.contains("expects exactly"),
            "unexpected error text: {err}"
        );
    }

    #[test]
    fn a_settings_run_with_an_issue_fails_even_with_the_expected_alerts() {
        let results = ConformResults::parse(RESULTS_FILE).unwrap();

        let err = settings_run_verdict(&results, &[PULSE_GUIDE_ALERT]).unwrap_err();

        assert!(err.contains("1 issue(s)"), "unexpected error text: {err}");
    }

    #[test]
    fn a_settings_run_with_an_error_fails_even_with_the_expected_alerts() {
        let results = ConformResults {
            errors: vec![("PulseGuide".to_owned(), "NOT_IMPLEMENTED".to_owned())],
            ..alerts(&[PULSE_GUIDE_ALERT])
        };

        let err = settings_run_verdict(&results, &[PULSE_GUIDE_ALERT]).unwrap_err();

        assert!(err.contains("1 error(s)"), "unexpected error text: {err}");
    }

    const URL: &str = "http://127.0.0.1:1/api/v1/telescope/0";

    #[test]
    fn a_full_run_with_a_zero_exit_and_an_empty_results_file_passes() {
        full_run_verdict(URL, "exit status: 0", true, Ok(ConformResults::default())).unwrap();
    }

    #[test]
    fn a_full_run_with_only_alerts_is_reported_as_narrowed() {
        let err = full_run_verdict(
            URL,
            "exit status: 1",
            false,
            Ok(alerts(&[PULSE_GUIDE_ALERT])),
        )
        .unwrap_err();

        assert!(
            err.contains("narrowed by configuration alerts"),
            "unexpected error text: {err}"
        );
    }

    #[test]
    fn a_full_run_with_issues_names_them() {
        let err = full_run_verdict(
            URL,
            "exit status: 2",
            false,
            ConformResults::parse(RESULTS_FILE),
        )
        .unwrap_err();

        assert!(
            err.contains("1 issue(s)") && err.contains("Slewed 12.3 arc seconds away"),
            "unexpected error text: {err}"
        );
    }

    #[test]
    fn a_full_run_with_errors_names_them_rather_than_a_narrowing() {
        let results = ConformResults {
            errors: vec![("PulseGuide".to_owned(), "NOT_IMPLEMENTED".to_owned())],
            ..alerts(&[PULSE_GUIDE_ALERT])
        };

        let err = full_run_verdict(URL, "exit status: 2", false, Ok(results)).unwrap_err();

        assert!(
            err.contains("1 error(s)") && !err.contains("narrowed"),
            "unexpected error text: {err}"
        );
    }

    /// `ConformU` exits with its count of findings, which Unix truncates to
    /// the low eight bits — 256 issues exit 0. The results file still lists
    /// them.
    #[test]
    fn a_full_run_whose_zero_exit_hides_listed_issues_fails() {
        let err = full_run_verdict(
            URL,
            "exit status: 0",
            true,
            ConformResults::parse(RESULTS_FILE),
        )
        .unwrap_err();

        assert!(err.contains("1 issue(s)"), "unexpected error text: {err}");
    }

    #[test]
    fn a_full_run_without_a_results_file_fails_even_on_a_zero_exit() {
        let err = full_run_verdict(
            URL,
            "exit status: 0",
            true,
            Err("cannot read /nowhere".to_owned()),
        )
        .unwrap_err();

        assert!(
            err.contains("left no readable results file") && err.contains("/nowhere"),
            "unexpected error text: {err}"
        );
    }

    #[test]
    fn a_full_run_with_a_non_zero_exit_fails_even_with_an_empty_results_file() {
        let err = full_run_verdict(URL, "exit status: 1", false, Ok(ConformResults::default()))
            .unwrap_err();

        assert!(
            err.contains("exited with exit status: 1"),
            "unexpected error text: {err}"
        );
    }

    /// The runners' wiring, exercised against a stand-in `conformu`: a shell
    /// script that records its arguments, writes a chosen results file when
    /// handed `--resultsfile` (only the conformance suites are), and exits
    /// with the status chosen for that suite. Unix-only because the stand-in
    /// is a `#!/bin/sh` script; the verdict logic above has no platform arm
    /// and runs everywhere.
    #[cfg(unix)]
    mod stand_in_conformu {
        use std::ffi::OsString;
        use std::os::unix::fs::PermissionsExt;
        use std::path::{Path, PathBuf};

        use tempfile::TempDir;

        use super::{complete_telescope_tests, PULSE_GUIDE_ALERT, SIDE_OF_PIER_READ_ALERT};
        use crate::conformu::{run_conformu, run_conformu_from_settings, ConformuRun};
        use crate::scratch;

        const NO_FINDINGS: &str = r#"{ "Errors": [], "Issues": [], "ConfigurationAlerts": [] }"#;

        /// These tests fork. A child forked by one test in the window between
        /// another test writing its script and closing it inherits that
        /// still-open descriptor, and the second test's exec then fails with
        /// `ETXTBSY` ("Text file busy"). Serialising each write-then-spawn
        /// sequence removes the overlap.
        static ONE_AT_A_TIME: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

        /// How the stand-in behaves: the protocol suite's exit status, then
        /// the conformance suite's, the results file it writes (`None` writes
        /// none), and a line it prints to stdout on every run (empty prints
        /// nothing).
        struct Behaviour<'a> {
            protocol_exit: i32,
            conformance_exit: i32,
            results: Option<&'a str>,
            stdout: &'a str,
        }

        /// A `conformu` that appends its arguments to `args.log` beside itself
        /// (one line per invocation) and behaves as `behaviour` says. The
        /// guard owns the script's directory.
        fn stand_in(behaviour: &Behaviour) -> (TempDir, PathBuf) {
            let dir = scratch::new_dir("stand-in-conformu-").unwrap();
            let path = dir.path().join("conformu");
            let log = dir.path().join("args.log");
            let write_results = behaviour
                .results
                .map(|json| format!("printf '%s' '{json}' > \"$2\"; "))
                .unwrap_or_default();
            let print_stdout = if behaviour.stdout.is_empty() {
                String::new()
            } else {
                format!("printf '%s\\n' '{}'\n", behaviour.stdout)
            };
            let script = format!(
                "#!/bin/sh\n\
                 printf '%s\\n' \"$*\" >> '{log}'\n\
                 {print_stdout}\
                 code={protocol}\n\
                 while [ $# -gt 0 ]; do\n\
                 if [ \"$1\" = --resultsfile ]; then {write_results}code={conformance}; fi\n\
                 shift\n\
                 done\n\
                 exit $code\n",
                log = log.display(),
                protocol = behaviour.protocol_exit,
                conformance = behaviour.conformance_exit,
            );
            std::fs::write(&path, script).unwrap();
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
            (dir, path)
        }

        /// The argument lines the stand-in recorded, in call order (none if
        /// it never ran).
        fn invocations(dir: &TempDir) -> Vec<String> {
            std::fs::read_to_string(dir.path().join("args.log"))
                .map(|log| log.lines().map(str::to_owned).collect())
                .unwrap_or_default()
        }

        /// A results file carrying exactly these configuration alerts.
        fn results_with_alerts(messages: &[&str]) -> String {
            let alerts: Vec<serde_json::Value> = messages
                .iter()
                .map(|m| serde_json::json!({ "Key": "Conform configuration", "Value": m }))
                .collect();
            serde_json::json!({ "Errors": [], "Issues": [], "ConfigurationAlerts": alerts })
                .to_string()
        }

        /// A settings file the pre-flight check accepts, in a scratch
        /// directory the guard owns.
        fn settings_file() -> (TempDir, PathBuf) {
            let dir = scratch::new_dir("stand-in-settings-").unwrap();
            let path = dir.path().join("bridge.json");
            let settings = serde_json::json!({
                "SettingsCompatibilityVersion": 1,
                "TelescopeTests": complete_telescope_tests(),
            });
            std::fs::write(&path, settings.to_string()).unwrap();
            (dir, path)
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
        async fn run_conformu_hands_settings_results_and_the_device_url_to_the_suites() {
            let _serial = ONE_AT_A_TIME.lock().await;
            let (dir, conformu) = stand_in(&Behaviour {
                protocol_exit: 0,
                conformance_exit: 0,
                results: Some(NO_FINDINGS),
                stdout: "",
            });
            let _env = ConformuPath::set(Some(&conformu));

            let outcome = run_conformu("telescope", "http://127.0.0.1:1/", 0, None)
                .await
                .unwrap();

            assert_eq!(outcome, ConformuRun::Passed);
            let calls = invocations(&dir);
            assert_eq!(calls.len(), 2, "{calls:?}");
            assert!(
                calls[0].starts_with("alpacaprotocol --settingsfile ")
                    && calls[0]
                        .ends_with("/conformu-settings.json http://127.0.0.1:1/api/v1/telescope/0"),
                "{}",
                calls[0]
            );
            assert!(
                calls[1].starts_with("conformance --settingsfile ")
                    && calls[1].contains("/conformu-settings.json --resultsfile ")
                    && calls[1].ends_with(
                        "/conformance-results.json http://127.0.0.1:1/api/v1/telescope/0"
                    ),
                "{}",
                calls[1]
            );
        }

        #[tokio::test]
        async fn run_conformu_from_settings_passes_with_exactly_the_expected_alerts() {
            let _serial = ONE_AT_A_TIME.lock().await;
            let results = results_with_alerts(&[PULSE_GUIDE_ALERT]);
            let (dir, conformu) = stand_in(&Behaviour {
                protocol_exit: 0,
                conformance_exit: 1,
                results: Some(&results),
                stdout: "",
            });
            let (_settings_dir, settings) = settings_file();
            let _env = ConformuPath::set(Some(&conformu));

            let outcome = run_conformu_from_settings(&settings, &[PULSE_GUIDE_ALERT])
                .await
                .unwrap();

            assert_eq!(outcome, ConformuRun::Passed);
            let calls = invocations(&dir);
            let settings_arg = format!("--settingsfile {}", settings.display());
            assert_eq!(calls.len(), 2, "{calls:?}");
            assert_eq!(calls[0], format!("alpacaprotocol-settings {settings_arg}"));
            assert!(
                calls[1].starts_with(&format!(
                    "conformance-settings {settings_arg} --resultsfile "
                )) && calls[1].ends_with("/conformance-results.json"),
                "{}",
                calls[1]
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
                let settings_run = run_conformu_from_settings(Path::new("/nowhere.json"), &[])
                    .await
                    .unwrap();
                assert_eq!(
                    (url_run, settings_run),
                    (ConformuRun::Skipped, ConformuRun::Skipped)
                );
            }
        }

        #[tokio::test]
        async fn a_full_run_narrowed_by_alerts_fails() {
            let _serial = ONE_AT_A_TIME.lock().await;
            let results = results_with_alerts(&[PULSE_GUIDE_ALERT, SIDE_OF_PIER_READ_ALERT]);
            let (_dir, conformu) = stand_in(&Behaviour {
                protocol_exit: 0,
                conformance_exit: 2,
                results: Some(&results),
                stdout: "",
            });
            let _env = ConformuPath::set(Some(&conformu));

            let err = run_conformu("telescope", "http://127.0.0.1:1", 0, None)
                .await
                .unwrap_err();

            assert!(
                err.to_string().contains("narrowed by configuration alerts"),
                "unexpected error text: {err}"
            );
        }

        #[tokio::test]
        async fn a_full_run_fails_when_the_protocol_suite_exits_non_zero() {
            let _serial = ONE_AT_A_TIME.lock().await;
            let (dir, conformu) = stand_in(&Behaviour {
                protocol_exit: 1,
                conformance_exit: 0,
                results: Some(NO_FINDINGS),
                stdout: "",
            });
            let _env = ConformuPath::set(Some(&conformu));

            let err = run_conformu("telescope", "http://127.0.0.1:1", 0, None)
                .await
                .unwrap_err();

            assert!(
                err.to_string()
                    .contains("`alpacaprotocol` exited with exit status: 1"),
                "unexpected error text: {err}"
            );
            assert_eq!(
                invocations(&dir).len(),
                1,
                "the conformance suite must not run"
            );
        }

        /// The exit status `ConformU` returns is its count of findings, which
        /// Unix truncates to eight bits, so a zero exit does not make a run
        /// clean: the results file decides.
        #[tokio::test]
        async fn a_full_run_with_a_zero_exit_but_listed_issues_fails() {
            let _serial = ONE_AT_A_TIME.lock().await;
            let results = serde_json::json!({
                "Errors": [],
                "Issues": [ { "Key": "SlewToTarget", "Value": "wrong" } ],
                "ConfigurationAlerts": [],
            })
            .to_string();
            let (_dir, conformu) = stand_in(&Behaviour {
                protocol_exit: 0,
                conformance_exit: 0,
                results: Some(&results),
                stdout: "",
            });
            let _env = ConformuPath::set(Some(&conformu));

            let err = run_conformu("telescope", "http://127.0.0.1:1", 0, None)
                .await
                .unwrap_err();

            assert!(
                err.to_string().contains("1 issue(s)"),
                "unexpected error text: {err}"
            );
        }

        #[tokio::test]
        async fn a_full_run_without_a_results_file_fails_on_a_zero_exit() {
            let _serial = ONE_AT_A_TIME.lock().await;
            let (_dir, conformu) = stand_in(&Behaviour {
                protocol_exit: 0,
                conformance_exit: 0,
                results: None,
                stdout: "",
            });
            let _env = ConformuPath::set(Some(&conformu));

            let err = run_conformu("telescope", "http://127.0.0.1:1", 0, None)
                .await
                .unwrap_err();

            assert!(
                err.to_string().contains("left no readable results file"),
                "unexpected error text: {err}"
            );
        }

        #[tokio::test]
        async fn a_full_run_with_a_non_zero_exit_fails_on_an_empty_results_file() {
            let _serial = ONE_AT_A_TIME.lock().await;
            let (_dir, conformu) = stand_in(&Behaviour {
                protocol_exit: 0,
                conformance_exit: 1,
                results: Some(NO_FINDINGS),
                stdout: "",
            });
            let _env = ConformuPath::set(Some(&conformu));

            let err = run_conformu("telescope", "http://127.0.0.1:1", 0, None)
                .await
                .unwrap_err();

            assert!(
                err.to_string()
                    .contains("although its results file lists no error, issue or alert"),
                "unexpected error text: {err}"
            );
        }

        #[tokio::test]
        async fn a_settings_run_with_a_clean_exit_but_no_expected_alert_fails() {
            let _serial = ONE_AT_A_TIME.lock().await;
            let (_dir, conformu) = stand_in(&Behaviour {
                protocol_exit: 0,
                conformance_exit: 0,
                results: Some(NO_FINDINGS),
                stdout: "",
            });
            let (_settings_dir, settings) = settings_file();
            let _env = ConformuPath::set(Some(&conformu));

            let err = run_conformu_from_settings(&settings, &[PULSE_GUIDE_ALERT])
                .await
                .unwrap_err();

            assert!(
                err.to_string().contains("expects exactly"),
                "unexpected error text: {err}"
            );
        }

        /// Ten issues fail the run although `ConformU`'s console summary for
        /// them, which the stand-in prints, contains the text `0 issues, 0
        /// errors and`: the verdict counts the results file's `Issues`, never
        /// the console.
        #[tokio::test]
        async fn a_settings_run_with_ten_issues_fails() {
            let _serial = ONE_AT_A_TIME.lock().await;
            let issues: Vec<serde_json::Value> = (0..10)
                .map(|n| serde_json::json!({ "Key": format!("Test{n}"), "Value": "wrong" }))
                .collect();
            let results = serde_json::json!({
                "Errors": [],
                "Issues": issues,
                "ConfigurationAlerts": [
                    { "Key": "Conform configuration", "Value": PULSE_GUIDE_ALERT }
                ],
            })
            .to_string();
            let (_dir, conformu) = stand_in(&Behaviour {
                protocol_exit: 0,
                conformance_exit: 11,
                results: Some(&results),
                stdout: "Your device had 10 issues, 0 errors and 1 configuration alert",
            });
            let (_settings_dir, settings) = settings_file();
            let _env = ConformuPath::set(Some(&conformu));

            let err = run_conformu_from_settings(&settings, &[PULSE_GUIDE_ALERT])
                .await
                .unwrap_err();

            assert!(
                err.to_string().contains("10 issue(s)"),
                "unexpected error text: {err}"
            );
        }

        #[tokio::test]
        async fn a_settings_run_with_alerts_beyond_the_expected_set_fails() {
            let _serial = ONE_AT_A_TIME.lock().await;
            let results = results_with_alerts(&[PULSE_GUIDE_ALERT, SIDE_OF_PIER_READ_ALERT]);
            let (_dir, conformu) = stand_in(&Behaviour {
                protocol_exit: 0,
                conformance_exit: 2,
                results: Some(&results),
                stdout: "",
            });
            let (_settings_dir, settings) = settings_file();
            let _env = ConformuPath::set(Some(&conformu));

            let err = run_conformu_from_settings(&settings, &[PULSE_GUIDE_ALERT])
                .await
                .unwrap_err();

            assert!(
                err.to_string().contains("expects exactly"),
                "unexpected error text: {err}"
            );
        }

        #[tokio::test]
        async fn a_settings_run_fails_when_the_protocol_suite_exits_non_zero() {
            let _serial = ONE_AT_A_TIME.lock().await;
            let results = results_with_alerts(&[PULSE_GUIDE_ALERT]);
            let (dir, conformu) = stand_in(&Behaviour {
                protocol_exit: 1,
                conformance_exit: 1,
                results: Some(&results),
                stdout: "",
            });
            let (_settings_dir, settings) = settings_file();
            let _env = ConformuPath::set(Some(&conformu));

            let err = run_conformu_from_settings(&settings, &[PULSE_GUIDE_ALERT])
                .await
                .unwrap_err();

            assert!(
                err.to_string()
                    .contains("`alpacaprotocol-settings` exited with exit status: 1"),
                "unexpected error text: {err}"
            );
            assert_eq!(
                invocations(&dir).len(),
                1,
                "the conformance suite must not run"
            );
        }

        #[tokio::test]
        async fn a_settings_run_without_a_results_file_fails() {
            let _serial = ONE_AT_A_TIME.lock().await;
            let (_dir, conformu) = stand_in(&Behaviour {
                protocol_exit: 0,
                conformance_exit: 1,
                results: None,
                stdout: "",
            });
            let (_settings_dir, settings) = settings_file();
            let _env = ConformuPath::set(Some(&conformu));

            let err = run_conformu_from_settings(&settings, &[PULSE_GUIDE_ALERT])
                .await
                .unwrap_err();

            assert!(
                err.to_string().contains("left no readable results file"),
                "unexpected error text: {err}"
            );
        }

        #[tokio::test]
        async fn an_incomplete_telescope_tests_dictionary_fails_before_conformu_starts() {
            let _serial = ONE_AT_A_TIME.lock().await;
            let (dir, conformu) = stand_in(&Behaviour {
                protocol_exit: 0,
                conformance_exit: 0,
                results: Some(NO_FINDINGS),
                stdout: "",
            });
            let settings_dir = scratch::new_dir("stand-in-settings-").unwrap();
            let settings = settings_dir.path().join("bridge.json");
            std::fs::write(
                &settings,
                r#"{ "SettingsCompatibilityVersion": 1, "TelescopeTests": {} }"#,
            )
            .unwrap();
            let _env = ConformuPath::set(Some(&conformu));

            let err = run_conformu_from_settings(&settings, &[])
                .await
                .unwrap_err();

            assert!(
                err.to_string().contains("\"CanMoveAxis\""),
                "unexpected error text: {err}"
            );
            assert!(invocations(&dir).is_empty(), "ConformU must not have run");
        }
    }
}
