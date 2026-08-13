//! Shared doctor-subcommand smoke: the fixture and steps behind every
//! service suite's two `doctor` scenarios (docs/services/doctor.md
//! §Per-service doctors) — a valid config yields a clean report, an
//! unknown key yields a failing report that names it. The runner's deep
//! behavior (rendering, absent files, SDK outcomes, exit-code mapping) is
//! unit-tested in `rusty-photon-doctor-checks`; these scenarios close the
//! loop through the real binary, its CLI, and its own typed load path.

use std::path::PathBuf;
use std::process::Output;

use tempfile::TempDir;

/// The key the unknown-key scenario injects at the top level of the
/// service's valid config. Chosen to collide with no real config shape.
pub const UNKNOWN_KEY: &str = "doctor_smoke_unknown_key";

/// Per-scenario smoke state; embed one (via `#[derive(Default)]`) in the
/// suite's `World`.
#[derive(Debug, Default)]
pub struct DoctorSmokeState {
    /// Owns the scratch directory the staged config lives in.
    pub config_dir: Option<TempDir>,
    pub config_path: Option<PathBuf>,
    pub output: Option<Output>,
}

/// Contract the [`doctor_smoke_steps!`](crate::doctor_smoke_steps) macro
/// programs against. Implementors supply only the genuinely
/// service-specific part: a config JSON their own typed load path accepts.
pub trait DoctorSmokeWorld {
    /// Mutable access to the embedded [`DoctorSmokeState`].
    fn doctor_smoke(&mut self) -> &mut DoctorSmokeState;

    /// A complete config JSON that this service's own load path parses
    /// cleanly (`deny_unknown_fields` included). The tls-auth smoke's
    /// base config plus a plain `server` block is usually right.
    fn valid_config(&self) -> serde_json::Value;
}

/// Write `config` into a scratch file owned by `state`.
pub fn stage_config(state: &mut DoctorSmokeState, config: &serde_json::Value) {
    let dir = state
        .config_dir
        .get_or_insert_with(|| TempDir::new().unwrap());
    let path = dir.path().join("doctor-smoke-config.json");
    std::fs::write(&path, config.to_string()).unwrap();
    state.config_path = Some(path);
}

/// Run `<binary> doctor --json --config <staged>` and record the output.
pub async fn run_doctor(state: &mut DoctorSmokeState, package_name: &str) {
    let path = state
        .config_path
        .clone()
        .expect("no config staged for the doctor run");
    let output = crate::run_once_async(
        package_name,
        &["doctor", "--json", "--config", path.to_str().unwrap()],
        None,
    )
    .await;
    state.output = Some(output);
}

/// The recorded run's parsed report plus its exit code.
fn report(state: &DoctorSmokeState) -> (serde_json::Value, Option<i32>) {
    let output = state.output.as_ref().expect("doctor was not run");
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    let report: serde_json::Value = serde_json::from_str(&stdout).unwrap_or_else(|e| {
        panic!("doctor stdout is not a JSON report: {e}\nstdout: {stdout}\nstderr: {stderr}")
    });
    (report, output.status.code())
}

/// The `config.full-shape` check object out of a parsed report.
fn full_shape_check(report: &serde_json::Value) -> &serde_json::Value {
    report["checks"]
        .as_array()
        .expect("report has no checks array")
        .iter()
        .find(|c| c["name"] == "config.full-shape")
        .unwrap_or_else(|| panic!("no config.full-shape check in {report}"))
}

/// Exit 0, `mode: service`, and an `ok` `config.full-shape`.
pub fn assert_report_clean(state: &DoctorSmokeState) {
    let (report, code) = report(state);
    assert_eq!(
        code,
        Some(0),
        "doctor exit code on a valid config\n{report}"
    );
    assert_eq!(
        report.get("mode").and_then(serde_json::Value::as_str),
        Some("service"),
        "{report}"
    );
    let check = full_shape_check(&report);
    assert_eq!(check["status"], "ok", "{report}");
}

/// Exit 1 and a `fail` `config.full-shape` whose detail names
/// [`UNKNOWN_KEY`].
pub fn assert_report_names_unknown_key(state: &DoctorSmokeState) {
    let (report, code) = report(state);
    assert_eq!(
        code,
        Some(1),
        "doctor exit code on an unknown key\n{report}"
    );
    let check = full_shape_check(&report);
    assert_eq!(check["status"], "fail", "{report}");
    let detail = check["detail"].as_str().unwrap_or_default();
    assert!(
        detail.contains(UNKNOWN_KEY),
        "detail does not name the unknown key: {detail}"
    );
}

/// Expands the shared doctor smoke step definitions against `$world`,
/// which must implement [`DoctorSmokeWorld`]. Invoke once in the suite's
/// `doctor_steps.rs`:
///
/// ```rust,ignore
/// use crate::world::MyWorld;
///
/// bdd_infra::doctor_smoke_steps!(MyWorld);
/// ```
///
/// `$world` is an `ident` for the same reason as
/// [`tls_auth_smoke_steps!`](crate::tls_auth_smoke_steps): cucumber's
/// attribute macro cannot see through a `ty` metavariable's invisible
/// group.
///
/// The generated steps match these scenarios (service-neutral wording, so
/// the feature file is identical across services):
///
/// ```gherkin
/// Scenario: A valid config file yields a clean report
///   Given this service's valid config file staged for doctor
///   When the doctor subcommand runs
///   Then the doctor report is clean
///
/// Scenario: An unknown config key fails the report and is named
///   Given this service's valid config file with an unknown key added
///   When the doctor subcommand runs
///   Then the doctor report fails naming the unknown key
/// ```
#[macro_export]
macro_rules! doctor_smoke_steps {
    ($world:ident) => {
        #[::cucumber::given("this service's valid config file staged for doctor")]
        fn doctor_smoke_stage_valid(world: &mut $world) {
            use $crate::doctor_smoke::DoctorSmokeWorld as _;
            let config = world.valid_config();
            $crate::doctor_smoke::stage_config(world.doctor_smoke(), &config);
        }

        #[::cucumber::given("this service's valid config file with an unknown key added")]
        fn doctor_smoke_stage_unknown_key(world: &mut $world) {
            use $crate::doctor_smoke::DoctorSmokeWorld as _;
            let mut config = world.valid_config();
            config[$crate::doctor_smoke::UNKNOWN_KEY] = true.into();
            $crate::doctor_smoke::stage_config(world.doctor_smoke(), &config);
        }

        #[::cucumber::when("the doctor subcommand runs")]
        async fn doctor_smoke_run(world: &mut $world) {
            use $crate::doctor_smoke::DoctorSmokeWorld as _;
            $crate::doctor_smoke::run_doctor(world.doctor_smoke(), env!("CARGO_PKG_NAME")).await;
        }

        #[::cucumber::then("the doctor report is clean")]
        fn doctor_smoke_clean(world: &mut $world) {
            use $crate::doctor_smoke::DoctorSmokeWorld as _;
            $crate::doctor_smoke::assert_report_clean(world.doctor_smoke());
        }

        #[::cucumber::then("the doctor report fails naming the unknown key")]
        fn doctor_smoke_names_unknown_key(world: &mut $world) {
            use $crate::doctor_smoke::DoctorSmokeWorld as _;
            $crate::doctor_smoke::assert_report_names_unknown_key(world.doctor_smoke());
        }
    };
}
