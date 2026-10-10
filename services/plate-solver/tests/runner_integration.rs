//! End-to-end integration tests for `AstapCliRunner::solve()`.
//!
//! Each test drives the full solve pipeline (`build_command` → spawn under
//! supervision → parse `.wcs`) against `mock_astap` configured for a
//! specific failure mode. These exercise the `solve()` orchestrator's
//! branches that unit tests can't reach without spawning a subprocess
//! (`ExitStatus`, `NoWcs`, `MalformedWcs`, success).
//!
//! `MOCK_ASTAP_MODE` is set per-test on the spawned child via the
//! `AstapCliRunner::with_env` builder, not via `std::env::set_var`,
//! so concurrent tests in the same process don't race.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
// Curated test-scope allow list — documented in the root Cargo.toml [workspace.lints] block.
#![allow(
    clippy::needless_pass_by_ref_mut,
    clippy::needless_pass_by_value,
    clippy::unused_async,
    clippy::unused_async_trait_impl,
    clippy::used_underscore_binding,
    clippy::significant_drop_tightening,
    clippy::significant_drop_in_scrutinee,
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss,
    clippy::cast_precision_loss,
    clippy::cast_possible_wrap,
    clippy::suboptimal_flops,
    clippy::too_many_lines,
    clippy::option_if_let_else,
    clippy::match_same_arms,
    clippy::float_cmp,
    clippy::similar_names,
    clippy::struct_excessive_bools
)]

use plate_solver::runner::wcs::read_wcs_sidecar;
use plate_solver::{AstapCliRunner, AstapRunner, RunnerError, SolveRequest};
use std::path::PathBuf;
use std::time::Duration;
use tempfile::TempDir;
use tokio::fs;

fn mock_astap_path() -> PathBuf {
    if let Ok(p) = std::env::var("MOCK_ASTAP_BINARY") {
        let path = PathBuf::from(p);
        if path.exists() {
            return path;
        }
    }
    if let Some(p) = option_env!("CARGO_BIN_EXE_mock_astap") {
        let path = PathBuf::from(p);
        if path.exists() {
            return path;
        }
    }
    panic!(
        "mock_astap binary not found. Tried MOCK_ASTAP_BINARY env var, then \
         CARGO_BIN_EXE_mock_astap. Run `cargo build --tests -p plate-solver`."
    )
}

fn runner(mode: &str) -> (AstapCliRunner, TempDir) {
    let dir = TempDir::new().expect("tempdir");
    let runner = AstapCliRunner::new(mock_astap_path(), dir.path().to_path_buf())
        .with_env("MOCK_ASTAP_MODE", mode);
    (runner, dir)
}

const fn req(fits_path: PathBuf) -> SolveRequest {
    SolveRequest {
        fits_path,
        ra_hint: None,
        dec_hint: None,
        fov_hint_deg: None,
        search_radius_deg: None,
        timeout: Duration::from_secs(5),
    }
}

#[tokio::test]
async fn happy_path_returns_solve_outcome() {
    let (runner, dir) = runner("normal");
    let fits = dir.path().join("test.fits");
    // mock_astap doesn't actually read the FITS, but builds the .wcs
    // sidecar path from -f arg, so the parent directory must exist.
    fs::write(&fits, b"placeholder").await.unwrap();

    let outcome = runner.solve(req(fits.clone())).await.unwrap();

    // Matches the canned values mock_astap writes (see CANNED_WCS).
    assert!((outcome.ra_center - 10.6848).abs() < 1e-6);
    assert!((outcome.dec_center - 41.2690).abs() < 1e-6);
    assert!((outcome.pixel_scale_arcsec - 1.05).abs() < 1e-2);
    assert!((outcome.rotation_deg - 12.3).abs() < 1e-6);
    let matrix = outcome.wcs_matrix.unwrap();
    assert!((matrix.crpix1 - 512.0).abs() < 1e-9);
    assert!((matrix.crpix2 - 384.0).abs() < 1e-9);
    assert!((matrix.cd1_1 - -0.000_284_972).abs() < 1e-12);
    assert!((matrix.cd1_2 - -0.000_062_134).abs() < 1e-12);
    assert!((matrix.cd2_1 - -0.000_062_134).abs() < 1e-12);
    assert!((matrix.cd2_2 - 0.000_284_972).abs() < 1e-12);

    // Sidecar should also round-trip through the parser directly.
    let wcs_path = fits.with_extension("wcs");
    assert!(wcs_path.exists(), "mock_astap should have written a .wcs");
    let _ = read_wcs_sidecar(&wcs_path).expect("canned .wcs parses");
}

#[tokio::test]
async fn exit_failure_maps_to_exit_status_error() {
    let (runner, dir) = runner("exit_failure");
    let fits = dir.path().join("test.fits");
    fs::write(&fits, b"placeholder").await.unwrap();

    let err = runner.solve(req(fits)).await.unwrap_err();
    match err {
        RunnerError::ExitStatus {
            status,
            stderr_tail,
        } => {
            assert_eq!(status, 1);
            assert!(
                stderr_tail.contains("simulated solve failure"),
                "expected stderr tail captured, got {stderr_tail:?}"
            );
        }
        other => panic!("expected ExitStatus, got {other:?}"),
    }
}

#[tokio::test]
async fn no_wcs_maps_to_no_wcs_error() {
    let (runner, dir) = runner("no_wcs");
    let fits = dir.path().join("test.fits");
    fs::write(&fits, b"placeholder").await.unwrap();

    let err = runner.solve(req(fits)).await.unwrap_err();
    assert!(
        matches!(err, RunnerError::NoWcs),
        "expected NoWcs, got {err:?}"
    );
}

#[tokio::test]
async fn malformed_wcs_maps_to_malformed_wcs_error() {
    let (runner, dir) = runner("malformed_wcs");
    let fits = dir.path().join("test.fits");
    fs::write(&fits, b"placeholder").await.unwrap();

    let err = runner.solve(req(fits)).await.unwrap_err();
    match err {
        RunnerError::MalformedWcs(msg) => {
            // mock_astap's malformed_wcs mode writes a .wcs missing CRVAL2.
            assert!(
                msg.to_uppercase().contains("CRVAL2"),
                "expected MalformedWcs to name the missing key, got {msg:?}"
            );
        }
        other => panic!("expected MalformedWcs, got {other:?}"),
    }
}

/// A child that answers the graceful signal at the deadline is a
/// `TimedOutTerminated` — the `(terminated)` the HTTP contract reports.
#[tokio::test]
async fn hang_maps_to_timed_out_terminated() {
    let (runner, dir) = runner("hang");
    let mut request = req(dir.path().join("test.fits"));
    request.timeout = Duration::from_millis(100);

    let err = runner.solve(request).await.unwrap_err();
    assert!(
        matches!(err, RunnerError::TimedOutTerminated),
        "expected TimedOutTerminated, got {err:?}"
    );
}

/// A child that ignores the graceful signal is force-killed after the grace:
/// `TimedOutKilled`. The deadline is a full second because the mock ignores
/// SIGTERM only once its `main` runs; a signal that beat it there would find
/// the default disposition and terminate it. Unix-only for the same reason
/// as the BDD scenario: the Windows mock's console handler is subject to
/// console-attach quirks.
#[cfg(unix)]
#[tokio::test]
async fn ignore_sigterm_maps_to_timed_out_killed() {
    let (runner, dir) = runner("ignore_sigterm");
    let mut request = req(dir.path().join("test.fits"));
    request.timeout = Duration::from_secs(1);

    let err = runner.solve(request).await.unwrap_err();
    assert!(
        matches!(err, RunnerError::TimedOutKilled),
        "expected TimedOutKilled, got {err:?}"
    );
}

/// A binary that cannot be started is a process error, not a solve failure.
#[tokio::test]
async fn unstartable_binary_maps_to_process_error() {
    let dir = TempDir::new().unwrap();
    let runner = AstapCliRunner::new(dir.path().join("no-such-astap"), dir.path().to_path_buf());
    let err = runner
        .solve(req(dir.path().join("test.fits")))
        .await
        .unwrap_err();
    assert!(
        matches!(
            err,
            RunnerError::Process(rusty_photon_process::Error::Spawn(_))
        ),
        "expected a spawn error, got {err:?}"
    );
}
