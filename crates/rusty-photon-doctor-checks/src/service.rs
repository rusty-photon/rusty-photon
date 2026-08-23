//! The shared per-service `doctor` runner (docs/services/doctor.md
//! §Per-service doctors).
//!
//! A service's subcommand is its clap variant plus a call here: the runner
//! owns the check assembly, rendering, and exit-code mapping, the service
//! hands in its typed config load and — on SDK-linking services — an
//! enumeration closure. Enumeration only, never an open: the subcommand
//! must stay safe to run by hand while the service holds its device.

use std::path::Path;

use crate::report::{Check, Report};

/// What SDK enumeration saw.
///
/// `Devices` carries the model names the SDK reports (empty = the SDK
/// works but sees nothing); `Error` is the SDK refusing to enumerate at
/// all, with an optional service-specific remedy (the qhy firmware helper,
/// a udev rule) where the service knows one.
pub enum SdkOutcome {
    Devices(Vec<String>),
    Error {
        detail: String,
        suggestion: Option<String>,
    },
}

/// The `config.full-shape` check: the file at `config_path` against the
/// service's own typed load path (`load` is that path, mapped to serde's
/// message on failure).
///
/// An absent file is `ok` — most services write their defaults on first
/// start, config-gated ones stay inert, and either way there is nothing to
/// validate (central doctor's inventory and `units.config-gated` checks
/// own that judgment).
pub fn full_shape_check(
    config_path: &Path,
    load: impl FnOnce(&Path) -> Result<(), String>,
) -> Check {
    if !config_path.exists() {
        return Check::ok(
            "config.full-shape",
            None,
            format!(
                "no config file at {}; nothing to validate",
                config_path.display()
            ),
        );
    }
    match load(config_path) {
        Ok(()) => Check::ok(
            "config.full-shape",
            None,
            "parses under the service's own config shape",
        ),
        Err(message) => Check::fail(
            "config.full-shape",
            None,
            message,
            Some(format!(
                "fix {} — the service will refuse to start over this",
                config_path.display()
            )),
        ),
    }
}

/// The `hardware.sdk-devices` check.
///
/// Zero devices is a `warn`, not a `fail`: this binary cannot see unit
/// state, so unplugged-on-purpose and unplugged-by-accident are
/// indistinguishable here — central doctor's unit-aware hardware checks
/// carry that judgment.
#[must_use]
pub fn sdk_devices_check(outcome: SdkOutcome) -> Check {
    match outcome {
        SdkOutcome::Devices(models) if models.is_empty() => Check::warn(
            "hardware.sdk-devices",
            None,
            "the SDK enumerated zero devices",
            Some("connect and power the device if tonight needs it".to_string()),
        ),
        SdkOutcome::Devices(models) => Check::ok(
            "hardware.sdk-devices",
            None,
            format!(
                "the SDK sees {} device(s): {}",
                models.len(),
                models.join(", ")
            ),
        ),
        SdkOutcome::Error { detail, suggestion } => Check::fail(
            "hardware.sdk-devices",
            None,
            format!("SDK enumeration failed: {detail}"),
            suggestion,
        ),
    }
}

/// Run a service's `doctor` subcommand: assemble the checks, render text
/// or JSON, and map to the shared exit-code contract.
///
/// The contract is 0 = no failures, 1 = at least one `fail`, 2 = the run
/// itself broke. Pure — the caller prints `output` (always
/// newline-terminated) to stdout and exits with `code` — so the whole
/// subcommand is testable without a process.
pub fn run(
    service: &str,
    version: &str,
    config_path: &Path,
    load: impl FnOnce(&Path) -> Result<(), String>,
    sdk: Option<SdkOutcome>,
    json: bool,
) -> (String, i32) {
    let mut checks = vec![full_shape_check(config_path, load)];
    if let Some(outcome) = sdk {
        checks.push(sdk_devices_check(outcome));
    }
    emit(service, version, config_path, checks, json)
}

/// Assemble `checks` into a `mode: service` report and render it.
///
/// The composition point for services whose subcommand carries extra
/// checks beyond the standard pair (qhy-camera's Windows DLL diagnostics).
/// Most services go through [`run`] instead.
#[must_use]
pub fn emit(
    service: &str,
    version: &str,
    config_path: &Path,
    checks: Vec<Check>,
    json: bool,
) -> (String, i32) {
    let report = Report::for_service(version, service, config_path.to_path_buf(), checks);
    let output = if json {
        match serde_json::to_string_pretty(&report) {
            Ok(mut json) => {
                json.push('\n');
                json
            }
            Err(error) => return (format!("cannot serialize report: {error}\n"), 2),
        }
    } else {
        crate::render::render(&report)
    };
    let code = i32::from(report.has_failures());
    (output, code)
}

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
#[allow(clippy::unreachable)]
mod tests {
    use super::*;
    use crate::report::Status;

    fn run_json(
        config_path: &Path,
        load: impl FnOnce(&Path) -> Result<(), String>,
        sdk: Option<SdkOutcome>,
    ) -> (Report, i32) {
        let (output, code) = run("qhy-camera", "0.9.0", config_path, load, sdk, true);
        (serde_json::from_str(&output).unwrap(), code)
    }

    #[test]
    fn test_absent_config_is_ok_and_the_load_is_never_called() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("qhy-camera.json");
        let (report, code) = run_json(
            &path,
            |_| unreachable!("load must not run for an absent file"),
            None,
        );
        assert_eq!(code, 0);
        assert_eq!(report.service.as_deref(), Some("qhy-camera"));
        assert_eq!(report.checks[0].name, "config.full-shape");
        assert_eq!(report.checks[0].status, Status::Ok);
        assert!(report.checks[0].detail.contains("nothing to validate"));
    }

    #[test]
    fn test_a_load_error_fails_and_names_the_message() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("qhy-camera.json");
        std::fs::write(&path, "{}").unwrap();
        let (report, code) = run_json(
            &path,
            |_| Err("unknown field `exposure_defaultt` at line 3".to_string()),
            None,
        );
        assert_eq!(code, 1);
        assert_eq!(report.checks[0].status, Status::Fail);
        assert!(report.checks[0].detail.contains("exposure_defaultt"));
        assert!(report.checks[0]
            .suggestion
            .as_deref()
            .unwrap()
            .contains("refuse to start"));
    }

    #[test]
    fn test_a_parsing_config_is_ok() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("ppba-driver.json");
        std::fs::write(&path, "{}").unwrap();
        let (report, code) = run_json(&path, |_| Ok(()), None);
        assert_eq!(code, 0);
        assert_eq!(report.checks.len(), 1, "no SDK check without a closure");
        assert_eq!(report.checks[0].status, Status::Ok);
    }

    #[test]
    fn test_sdk_enumeration_outcomes_map_to_ok_warn_fail() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("qhy-camera.json");

        let (report, code) = run_json(
            &path,
            |_| Ok(()),
            Some(SdkOutcome::Devices(vec![
                "QHY178M".to_string(),
                "QHY5III715C".to_string(),
            ])),
        );
        assert_eq!(code, 0);
        assert_eq!(report.checks[1].name, "hardware.sdk-devices");
        assert_eq!(report.checks[1].status, Status::Ok);
        assert!(report.checks[1].detail.contains("2 device(s)"));
        assert!(report.checks[1].detail.contains("QHY178M, QHY5III715C"));

        let (report, code) = run_json(&path, |_| Ok(()), Some(SdkOutcome::Devices(vec![])));
        assert_eq!(code, 0, "zero devices warns; warnings are not failures");
        assert_eq!(report.checks[1].status, Status::Warn);

        let (report, code) = run_json(
            &path,
            |_| Ok(()),
            Some(SdkOutcome::Error {
                detail: "InitQHYCCDResource returned QHYCCD_ERROR".to_string(),
                suggestion: Some("run rusty-photon-qhy-firmware-install".to_string()),
            }),
        );
        assert_eq!(code, 1);
        assert_eq!(report.checks[1].status, Status::Fail);
        assert!(report.checks[1].detail.contains("SDK enumeration failed"));
        assert!(report.checks[1]
            .suggestion
            .as_deref()
            .unwrap()
            .contains("firmware-install"));
    }

    #[test]
    fn test_text_output_renders_the_service_header() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("dsd-fp2.json");
        let (output, code) = run("dsd-fp2", "0.2.0", &path, |_| Ok(()), None, false);
        assert_eq!(code, 0);
        assert!(
            output.contains("rusty-photon-dsd-fp2 doctor 0.2.0"),
            "{output}"
        );
        assert!(output.contains("summary: 1 ok, 0 warn, 0 fail"), "{output}");
    }
}
