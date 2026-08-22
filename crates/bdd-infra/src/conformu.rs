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

use std::path::Path;
use std::process::Stdio;

use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::process::Command;

/// Outcome of [`run_conformu`].
#[derive(Debug, PartialEq, Eq)]
pub enum ConformuRun {
    /// `CONFORMU_PATH` was not set, so `ConformU` was not run. Callers treat this
    /// as a pass: the suite is inert unless `ConformU` is explicitly provided.
    Skipped,
    /// `ConformU` ran and reported success (zero exit status).
    Passed,
}

/// Run the ASCOM `ConformU` `conformance` suite against a running Alpaca device.
///
/// Equivalent to:
///
/// ```text
/// conformu conformance --settingsfile <settings_file> <base_url>/api/v1/<device_type>/<device_number>
/// ```
///
/// `device_type` is the lowercase Alpaca device-type URL segment (`"focuser"`,
/// `"camera"`, `"switch"`, `"telescope"`, `"rotator"`, `"covercalibrator"`,
/// `"observingconditions"`, `"safetymonitor"`). `base_url` is the device server
/// root (e.g. `http://127.0.0.1:PORT/`), typically `ServiceHandle::base_url`.
///
/// Returns [`ConformuRun::Skipped`] when `CONFORMU_PATH` is unset and
/// [`ConformuRun::Passed`] once both the `alpacaprotocol` and `conformance`
/// suites have passed.
///
/// # Errors
///
/// Returns an error if `ConformU` cannot be spawned, its output cannot be
/// read, or either suite exits non-zero — except an exit whose summary line
/// reports zero errors and zero issues (configuration alerts only), which is
/// accepted.
pub async fn run_conformu(
    device_type: &str,
    base_url: &str,
    device_number: u32,
    settings_file: Option<&Path>,
) -> Result<ConformuRun, Box<dyn std::error::Error + Send + Sync>> {
    let Some(conformu) = std::env::var_os("CONFORMU_PATH").filter(|v| !v.is_empty()) else {
        eprintln!("CONFORMU_PATH not set; skipping ConformU run for {device_type}/{device_number}");
        return Ok(ConformuRun::Skipped);
    };

    let device_url = format!(
        "{base}/api/v1/{device_type}/{device_number}",
        base = base_url.trim_end_matches('/'),
    );

    // Run both ConformU suites against the device, matching the upstream
    // ascom_alpaca::test runner (`ConformUTestBuilder::run`): `alpacaprotocol`
    // (Alpaca wire-protocol conformance) then `conformance` (full ASCOM
    // device-interface tests). Both must pass.
    for mode in ["alpacaprotocol", "conformance"] {
        run_mode(&conformu, mode, settings_file, Some(&device_url)).await?;
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
/// used by [`run_conformu`]) call `ConformU`'s `SetFullTest()`, which
/// force-enables every test — and some capability sets cannot satisfy the
/// full set. The worked example is a `CanPulseGuide = false` Telescope
/// (planetarium-bridge): the protocol suite's `PulseGuide` test polls
/// `IsPulseGuiding` as its completion check and records the spec-mandated
/// `NOT_IMPLEMENTED` answer as an error, so the test must be deselected —
/// which only the `*-settings` commands honor.
///
/// A deliberately omitted test produces a `ConformU` "configuration alert",
/// and alerts count into the exit code exactly like errors and issues. A
/// run whose only marks are configuration alerts is therefore accepted as a
/// pass here, detected via the summary line `ConformU` prints; errors and
/// issues still fail.
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
        run_mode(&conformu, mode, Some(settings_file), None).await?;
    }
    Ok(ConformuRun::Passed)
}

/// Run a single `ConformU` mode, streaming its output. `device_url` is the
/// positional device argument for the URL-based commands and `None` for the
/// `*-settings` commands (which read the device from the settings file).
/// Returns `Err` on a non-zero exit, except when the output's summary line
/// shows zero errors and zero issues — the exit code also counts
/// configuration alerts (deliberately deselected tests), which are not
/// device defects.
async fn run_mode(
    conformu: &std::ffi::OsStr,
    mode: &str,
    settings_file: Option<&Path>,
    device_url: Option<&str>,
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
    // `--settingsfile` is optional: services that need non-default ConformU
    // settings (timeouts, which test groups to run) pass a written file; the
    // rest run with ConformU's defaults.
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
        Ok(())
    } else if clean_except_alerts {
        println!(
            "[conformu {mode}] non-zero exit {status} accepted: the summary reported 0 issues \
             and 0 errors (configuration alerts only)"
        );
        Ok(())
    } else {
        Err(format!("ConformU `{mode}` exited with {status} testing {target}").into())
    }
}
