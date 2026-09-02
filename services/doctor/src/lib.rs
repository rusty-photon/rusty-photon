//! rusty-photon-doctor: read-only diagnosis of a multi-service install.
//!
//! Design: docs/services/doctor.md. Plan and ownership decisions:
//! docs/plans/service-config-doctor.md, ADR-016. This crate links no
//! service crate: it knows the derived catalog, the two shared server
//! shapes, and the known cross-reference blocks; every other byte of every
//! config file is opaque.

#![cfg_attr(coverage_nightly, feature(coverage_attribute))]
#![cfg_attr(
    test,
    allow(
        clippy::arithmetic_side_effects,
        clippy::as_conversions,
        clippy::indexing_slicing
    )
)]
// Curated test-scope allow list — documented in the root Cargo.toml [workspace.lints] block.
#![cfg_attr(
    test,
    allow(
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
        clippy::struct_excessive_bools,
    )
)]

pub mod aggregate;
pub mod catalog;
pub mod checks;
pub mod facts;
pub mod fix;
pub mod flip;
pub mod hardware;
pub mod provision;
pub mod render;
pub mod report;
pub mod scan;

use std::path::{Path, PathBuf};

use facts::PlatformFacts;
use report::Report;
use tracing::debug;

/// Resolve which config directory to diagnose (docs/services/doctor.md
/// §Config-root resolution).
///
/// The explicit flag (which must exist), else the packaged
/// `/etc/rusty-photon` symlink, else the platform default the services
/// themselves resolve — which may not exist yet on a fresh host and is
/// then diagnosed as empty.
///
/// # Errors
///
/// Returns a message if the explicit `--config-dir` is not a directory, if
/// the platform default cannot be resolved, or — on Unix — if the packaged
/// tree exists but is **unreadable** by this user. That last one is a hard
/// error, not a fall-through: the tree is owned by the `rusty-photon`
/// user, and silently diagnosing the invoking user's own empty config
/// directory instead would report seventeen missing configs on a
/// perfectly healthy rig.
pub fn resolve_config_dir(explicit: Option<PathBuf>) -> Result<PathBuf, String> {
    if let Some(dir) = explicit {
        if !dir.is_dir() {
            return Err(format!(
                "--config-dir {} does not exist or is not a directory",
                dir.display()
            ));
        }
        return Ok(dir);
    }
    #[cfg(unix)]
    {
        if let Some(resolved) = packaged_config_dir(Path::new("/etc/rusty-photon"))? {
            return Ok(resolved);
        }
    }
    let dir = rusty_photon_config::default_config_dir()
        .map_err(|e| format!("could not resolve a config directory: {e}"))?;
    debug!(dir = %dir.display(), "using the platform default config directory");
    Ok(dir)
}

/// Materialize an explicit `--config-dir` that does not exist yet
/// (docs/services/doctor.md §Config-root resolution).
///
/// Only `tls issue` calls this: issuance is the one command whose job is
/// creating material from nothing — the recommended ACME staging
/// rehearsal targets a scratch directory — while every other entry point
/// keeps rejecting a missing path, so a typo'd `--config-dir` never
/// silently operates on an empty directory.
///
/// # Errors
///
/// Returns a message if the path exists but is not a directory, or if
/// creating it fails.
pub fn ensure_explicit_config_dir(explicit: Option<&Path>) -> Result<(), String> {
    let Some(dir) = explicit else {
        return Ok(());
    };
    if dir.is_dir() {
        return Ok(());
    }
    if dir.symlink_metadata().is_ok() {
        return Err(format!(
            "--config-dir {} exists but is not a directory",
            dir.display()
        ));
    }
    std::fs::create_dir_all(dir)
        .map_err(|e| format!("could not create --config-dir {}: {e}", dir.display()))?;
    debug!(dir = %dir.display(), "created the explicit config directory");
    Ok(())
}

/// The packaged-tree leg of the resolution: `Ok(Some)` when the symlink
/// exists and is traversable, `Ok(None)` to fall through (absent, or
/// dangling — the packages were removed), `Err` when it exists but this
/// user cannot read it.
#[cfg(unix)]
fn packaged_config_dir(packaged: &Path) -> Result<Option<PathBuf>, String> {
    if packaged.symlink_metadata().is_err() {
        return Ok(None);
    }
    match std::fs::read_dir(packaged) {
        Ok(_) => {
            debug!(dir = %packaged.display(), "using the packaged config tree");
            Ok(Some(packaged.to_path_buf()))
        }
        Err(e) if e.kind() == std::io::ErrorKind::PermissionDenied => Err(format!(
            "{} exists but is not readable by this user — a packaged install's \
             config tree is owned by the rusty-photon user, so run doctor with \
             elevated privileges (e.g. sudo)",
            packaged.display()
        )),
        Err(e) => {
            debug!(dir = %packaged.display(), error = %e,
                   "packaged config tree unusable (dangling symlink?) — falling through");
            Ok(None)
        }
    }
}

/// Run the whole diagnosis: scan, check, resolve the public names, probe
/// the per-service doctors, report.
#[must_use]
pub fn diagnose(config_dir: PathBuf, facts: PlatformFacts) -> Report {
    let (ctx, mut checks) = diagnose_static(config_dir, facts);
    checks.extend(checks::dns_resolution(&ctx));
    checks.extend(aggregate::checks(&ctx));
    Report::new(env!("CARGO_PKG_VERSION"), ctx.mode, ctx.config_dir, checks)
}

/// The pure half of the diagnosis: everything except the per-service
/// aggregation probes and the DNS resolution check. The `--fix` loop
/// iterates on this — neither family plans a fix, so probing (HTTP
/// requests, shell-outs with SDK bus scans, resolver lookups that a
/// misconfigured resolver answers only by timeout) on every intermediate
/// round would be pure cost — and appends both once, on the final report.
fn diagnose_static(
    config_dir: PathBuf,
    facts: PlatformFacts,
) -> (checks::Context, Vec<report::Check>) {
    debug!(config_dir = %config_dir.display(), platform = ?facts.platform,
           units = facts.units.len(), "gathering context");
    let ctx = checks::Context::gather(config_dir, facts);
    let checks = checks::run_all(&ctx);
    (ctx, checks)
}

/// One fix can unlock the next (a freed default port makes another
/// collision fixable), so `--fix` iterates plan→apply to a fixpoint. The
/// cap is a runaway backstop, far above any real chain — ops are
/// idempotent, so even a pathological planner converges to no-ops.
const MAX_FIX_ROUNDS: usize = 4;

/// Diagnose, apply the machine-applicable fixes, re-diagnose — repeated
/// until a round plans nothing — and report the post-fix state plus what
/// was written.
///
/// The provisioning material pass (docs/services/doctor.md §Provisioning)
/// runs first: the `server.tls`/`server.auth` blocks the fix rounds write
/// must point at material that exists, and the `auth.absent` plan needs
/// `pki/credential` to hash.
///
/// # Errors
///
/// Returns a message if a provisioning step (CA, certificate, or
/// credential material; pki ownership alignment) or a fix write itself
/// fails — exit 2 territory. The diagnosis outcome is never an error; it
/// stays in the report.
pub fn diagnose_and_fix(config_dir: PathBuf, facts: PlatformFacts) -> Result<Report, String> {
    let mut applied = provision_material(&config_dir, &facts)?;
    for round in 0..MAX_FIX_ROUNDS {
        let (ctx, mut checks) = diagnose_static(config_dir.clone(), facts.clone());
        let planned: usize = checks.iter().map(|c| c.fixes.len()).sum();
        // The sentinel client-block wiring is provisioning-pass work, not a
        // check's fix plan — planned fresh each round so a second run plans
        // (and applies) nothing.
        let client_ops = provision::plan_client_wiring(&config_dir);
        if planned == 0 && client_ops.is_empty() {
            debug!(round, applied = applied.len(), "fix rounds converged");
            checks.extend(checks::dns_resolution(&ctx));
            checks.extend(aggregate::checks(&ctx));
            let report = Report::new(env!("CARGO_PKG_VERSION"), ctx.mode, ctx.config_dir, checks);
            return Ok(report.with_fixes_applied(applied));
        }
        let mut round_applied = fix::apply_fixes(&config_dir, &checks)?;
        round_applied.extend(fix::apply_ops(&config_dir, client_ops, false)?);
        if round_applied.is_empty() {
            // Planned targets were already gone (a concurrent edit landed
            // between diagnosis and apply). Nothing was written, but the
            // diagnosis in hand is stale now — loop so the returned report
            // is always a fresh post-state diagnosis.
            continue;
        }
        applied.extend(round_applied);
    }
    Ok(diagnose(config_dir, facts).with_fixes_applied(applied))
}

/// The flip orchestrator's verification pass (docs/services/doctor.md
/// §The flip orchestrator).
///
/// The flip check families plus the DNS resolution / hosts-line check,
/// over one context — the flip's report is scoped to the transition, and
/// a full `doctor` run stays the general health tool.
#[must_use]
pub fn flip_verification(ctx: &checks::Context) -> Vec<report::Check> {
    let mut checks = checks::flip_convergence(ctx);
    checks.extend(checks::dns_resolution(ctx));
    checks
}

/// The material half of the provisioning pass: CA-if-absent, a certificate
/// pair per installed service, and the observatory credential. Nothing is
/// created on a host with no installed services — an empty config
/// directory must stay empty. On an ACME install (`acme.json` present) no
/// self-signed material is created either: every service serves the
/// shared wildcard pair, which is `tls issue --acme`'s (and renewal's) to
/// mint, and self-signed certs would be unverifiable by the flipped
/// fleet's clients (issue #616). The credential is trust-model-agnostic
/// and is ensured either way.
fn provision_material(
    config_dir: &Path,
    facts: &PlatformFacts,
) -> Result<Vec<report::AppliedFix>, String> {
    let ctx = checks::Context::gather(config_dir.to_path_buf(), facts.clone());
    let services = ctx.installed_services();
    if services.is_empty() {
        debug!("no installed services; skipping the provisioning material pass");
        return Ok(Vec::new());
    }
    let mut applied = Vec::new();
    if provision::acme_active(config_dir) {
        debug!("acme.json present: the wildcard pair serves TLS; no self-signed material issued");
        // Every provisioning pass must still end aligned (§Ownership under
        // sudo) even though nothing is issued: a wildcard pair or acme.json
        // left root-owned by an earlier sudo action would otherwise stay
        // unreadable by the service user — `ensure_credential` aligns only
        // when it mints.
        provision::align_pki_ownership(config_dir)?;
    } else {
        applied = provision::ensure_material(config_dir, &services, &[], false)?;
    }
    let (_password, credential_applied) = provision::ensure_credential(config_dir)?;
    applied.extend(credential_applied);
    Ok(applied)
}

#[cfg(all(test, unix))]
#[cfg_attr(coverage_nightly, coverage(off))]
mod tests {
    use std::os::unix::fs::PermissionsExt;

    use super::*;

    #[test]
    fn test_packaged_dir_is_used_when_traversable() {
        let dir = tempfile::tempdir().unwrap();
        let resolved = packaged_config_dir(dir.path()).unwrap();
        assert_eq!(resolved, Some(dir.path().to_path_buf()));
    }

    #[test]
    fn test_absent_and_dangling_packaged_dirs_fall_through() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(
            packaged_config_dir(&dir.path().join("absent")).unwrap(),
            None
        );
        let dangling = dir.path().join("etc-rusty-photon");
        std::os::unix::fs::symlink(dir.path().join("gone"), &dangling).unwrap();
        assert_eq!(packaged_config_dir(&dangling).unwrap(), None);
    }

    #[test]
    fn test_unreadable_packaged_dir_demands_privileges() {
        let dir = tempfile::tempdir().unwrap();
        let sealed = dir.path().join("sealed");
        std::fs::create_dir(&sealed).unwrap();
        std::fs::set_permissions(&sealed, std::fs::Permissions::from_mode(0o000)).unwrap();
        let result = packaged_config_dir(&sealed);
        std::fs::set_permissions(&sealed, std::fs::Permissions::from_mode(0o755)).unwrap();
        if result.is_ok() {
            return; // running privileged — mode 000 is still readable by root
        }
        let err = result.unwrap_err();
        assert!(err.contains("sudo"), "{err}");
    }

    #[test]
    fn test_explicit_config_dir_must_exist() {
        let dir = tempfile::tempdir().unwrap();
        let err = resolve_config_dir(Some(dir.path().join("nope"))).unwrap_err();
        assert!(err.contains("--config-dir"), "{err}");
        let ok = resolve_config_dir(Some(dir.path().to_path_buf())).unwrap();
        assert_eq!(ok, dir.path());
    }
}

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
mod ensure_config_dir_tests {
    use super::*;

    #[test]
    fn test_ensure_explicit_config_dir_no_flag_is_a_no_op() {
        ensure_explicit_config_dir(None).unwrap();
    }

    #[test]
    fn test_ensure_explicit_config_dir_keeps_an_existing_directory() {
        let dir = tempfile::tempdir().unwrap();
        ensure_explicit_config_dir(Some(dir.path())).unwrap();
        assert!(dir.path().is_dir());
    }

    #[test]
    fn test_ensure_explicit_config_dir_creates_a_missing_path() {
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("rehearsal").join("nested");
        ensure_explicit_config_dir(Some(&target)).unwrap();
        assert!(target.is_dir());
    }

    #[test]
    fn test_ensure_explicit_config_dir_rejects_a_file() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("occupied");
        std::fs::write(&file, b"x").unwrap();
        let err = ensure_explicit_config_dir(Some(&file)).unwrap_err();
        assert!(err.contains("is not a directory"), "{err}");
    }
}
