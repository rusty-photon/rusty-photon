//! Real `AstapRunner` implementation: builds an `astap_cli` `Command`,
//! spawns it under the supervision module, and parses the resulting
//! `.wcs` sidecar.
//!
//! The argv-mapping behavior is unit-tested in this file. The spawn-based
//! end-to-end behavior (real `mock_astap` child + supervision arms) lives
//! in `tests/supervision_integration.rs`.

use super::wcs::read_wcs_sidecar;
use super::{AstapRunner, RunnerError, SolveOutcome, SolveRequest};
use crate::supervision::{spawn_with_deadline, SpawnOutcome};
use async_trait::async_trait;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use tokio::process::Command;

#[cfg(windows)]
const CREATE_NEW_PROCESS_GROUP: u32 = 0x0000_0200;

/// Wraps `astap_cli` invocations.
pub struct AstapCliRunner {
    binary_path: PathBuf,
    db_directory: PathBuf,
    extra_env: Vec<(String, String)>,
}

impl AstapCliRunner {
    #[must_use]
    pub const fn new(binary_path: PathBuf, db_directory: PathBuf) -> Self {
        Self {
            binary_path,
            db_directory,
            extra_env: Vec::new(),
        }
    }

    /// Add an environment variable to set on every spawned `astap_cli`
    /// child. Useful for operator-controlled tunables (locale, library
    /// paths) and for integration tests that drive `mock_astap`'s
    /// `MOCK_ASTAP_MODE` per-test without process-wide `env::set_var`
    /// races.
    #[must_use]
    pub fn with_env(mut self, key: impl Into<String>, value: impl Into<String>) -> Self {
        self.extra_env.push((key.into(), value.into()));
        self
    }

    /// Build the `Command` argv from a `SolveRequest` without spawning.
    /// Pure function; exercised by argv-mapping unit tests.
    #[must_use]
    pub fn build_command(&self, req: &SolveRequest) -> Command {
        let mut cmd = Command::new(&self.binary_path);
        cmd.arg("-f").arg(&req.fits_path);
        cmd.arg("-d").arg(&self.db_directory);
        cmd.arg("-wcs");

        if let Some(ra_deg) = req.ra_hint {
            // Wire format: decimal degrees (0–360). ASTAP `-ra` wants
            // decimal hours. Convert here, see design doc §"Hint Mapping".
            cmd.arg("-ra").arg(format!("{:.10}", ra_deg / 15.0));
        }
        if let Some(dec_deg) = req.dec_hint {
            // ASTAP `-spd` wants south-pole-distance = 90 + dec.
            cmd.arg("-spd").arg(format!("{:.10}", 90.0 + dec_deg));
        }
        if let Some(fov) = req.fov_hint_deg {
            cmd.arg("-fov").arg(format!("{fov:.10}"));
        }
        if let Some(r) = req.search_radius_deg {
            cmd.arg("-r").arg(format!("{r:.10}"));
        }

        cmd.stdout(Stdio::null());
        cmd.stderr(Stdio::piped());

        for (k, v) in &self.extra_env {
            cmd.env(k, v);
        }

        #[cfg(windows)]
        {
            // `tokio::process::Command::creation_flags` is an inherent
            // method on Windows; no `CommandExt` trait import needed
            // (importing the std-process one would just trip
            // unused_imports under -D warnings).
            cmd.creation_flags(CREATE_NEW_PROCESS_GROUP);
        }

        cmd
    }
}

#[async_trait]
impl AstapRunner for AstapCliRunner {
    async fn solve(&self, request: SolveRequest) -> Result<SolveOutcome, RunnerError> {
        let timeout = request.timeout;
        let fits_path = request.fits_path.clone();
        let cmd = self.build_command(&request);
        let outcome = spawn_with_deadline(cmd, timeout).await?;
        match outcome {
            SpawnOutcome::Exited {
                status,
                stderr_tail,
            } => {
                if !status.success() {
                    return Err(RunnerError::ExitStatus {
                        status: status.code().unwrap_or(-1),
                        stderr_tail,
                    });
                }
                let wcs_path = wcs_sidecar_path(&fits_path);
                if !wcs_path.exists() {
                    return Err(RunnerError::NoWcs);
                }
                read_wcs_sidecar(&wcs_path).map_err(|e| RunnerError::MalformedWcs(e.to_string()))
            }
            SpawnOutcome::TimedOutTerminated => Err(RunnerError::TimedOutTerminated),
            SpawnOutcome::TimedOutKilled => Err(RunnerError::TimedOutKilled),
        }
    }
}

/// Compute the `.wcs` sidecar path for a FITS file: replace the
/// trailing extension with `.wcs`. ASTAP writes the sidecar next to
/// the input. Single-extension forms (`.fits`, `.fit`, `.fz`) are
/// handled directly by `Path::with_extension`. Compound extensions
/// like `.fits.fz` are **not supported** here — `with_extension`
/// would yield `something.fits.wcs` rather than `something.wcs`.
/// Real fixtures using `.fits.fz` haven't surfaced; revisit if they
/// do.
fn wcs_sidecar_path(fits_path: &Path) -> PathBuf {
    fits_path.with_extension("wcs")
}

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
mod tests {
    use super::*;
    use std::time::Duration;

    fn req() -> SolveRequest {
        SolveRequest {
            fits_path: PathBuf::from("/data/lights/m31.fits"),
            ra_hint: None,
            dec_hint: None,
            fov_hint_deg: None,
            search_radius_deg: None,
            timeout: Duration::from_secs(30),
        }
    }

    fn argv(cmd: &Command) -> Vec<String> {
        cmd.as_std()
            .get_args()
            .map(|s| s.to_string_lossy().into_owned())
            .collect()
    }

    fn runner() -> AstapCliRunner {
        AstapCliRunner::new(
            PathBuf::from("/opt/astap/astap_cli"),
            PathBuf::from("/opt/astap/d05"),
        )
    }

    #[test]
    fn no_hints_produces_minimal_argv() {
        let cmd = runner().build_command(&req());
        assert_eq!(
            argv(&cmd),
            vec![
                "-f".to_string(),
                "/data/lights/m31.fits".to_string(),
                "-d".to_string(),
                "/opt/astap/d05".to_string(),
                "-wcs".to_string(),
            ]
        );
    }

    #[test]
    fn ra_hint_converts_degrees_to_hours() {
        let mut r = req();
        r.ra_hint = Some(10.6848); // M31 in degrees
        let cmd = runner().build_command(&r);
        let argv = argv(&cmd);
        let ra_idx = argv.iter().position(|a| a == "-ra").unwrap();
        let ra_val: f64 = argv[ra_idx + 1].parse().unwrap();
        // 10.6848 / 15 = 0.71232
        assert!((ra_val - 10.6848 / 15.0).abs() < 1e-9);
    }

    #[test]
    fn dec_hint_converts_to_south_pole_distance() {
        let mut r = req();
        r.dec_hint = Some(41.2690); // M31 in degrees
        let cmd = runner().build_command(&r);
        let argv = argv(&cmd);
        let spd_idx = argv.iter().position(|a| a == "-spd").unwrap();
        let spd_val: f64 = argv[spd_idx + 1].parse().unwrap();
        // 90 + 41.2690 = 131.2690
        assert!((spd_val - 131.2690).abs() < 1e-9);
    }

    #[test]
    fn fov_and_radius_pass_through() {
        let mut r = req();
        r.fov_hint_deg = Some(1.5);
        r.search_radius_deg = Some(5.0);
        let cmd = runner().build_command(&r);
        let argv = argv(&cmd);
        let fov_idx = argv.iter().position(|a| a == "-fov").unwrap();
        let fov: f64 = argv[fov_idx + 1].parse().unwrap();
        assert!((fov - 1.5).abs() < 1e-9);
        let r_idx = argv.iter().position(|a| a == "-r").unwrap();
        let radius: f64 = argv[r_idx + 1].parse().unwrap();
        assert!((radius - 5.0).abs() < 1e-9);
    }

    #[test]
    fn all_hints_present_in_full_argv() {
        let mut r = req();
        r.ra_hint = Some(10.6848);
        r.dec_hint = Some(41.2690);
        r.fov_hint_deg = Some(1.5);
        r.search_radius_deg = Some(5.0);
        let cmd = runner().build_command(&r);
        let argv = argv(&cmd);
        assert!(argv.contains(&"-ra".to_string()));
        assert!(argv.contains(&"-spd".to_string()));
        assert!(argv.contains(&"-fov".to_string()));
        assert!(argv.contains(&"-r".to_string()));
    }

    #[test]
    fn wcs_sidecar_path_replaces_fits_extension() {
        assert_eq!(
            wcs_sidecar_path(Path::new("/data/m31.fits")),
            PathBuf::from("/data/m31.wcs")
        );
        assert_eq!(
            wcs_sidecar_path(Path::new("/data/m31.fit")),
            PathBuf::from("/data/m31.wcs")
        );
        assert_eq!(
            wcs_sidecar_path(Path::new("/data/m31.fz")),
            PathBuf::from("/data/m31.wcs")
        );
    }

    #[test]
    fn wcs_sidecar_path_compound_extension_not_supported() {
        // Documents the known non-support for compound extensions:
        // `with_extension` only replaces the last component, so
        // `.fits.fz` yields `.fits.wcs`, not `.wcs`. The behavior is
        // intentional per wcs_sidecar_path's docstring.
        assert_eq!(
            wcs_sidecar_path(Path::new("/data/m31.fits.fz")),
            PathBuf::from("/data/m31.fits.wcs")
        );
    }
}
