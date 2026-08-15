//! Bazel test-sharding support for cucumber BDD suites.
//!
//! Bazel splits a test target into `shard_count` separate test actions, each
//! running the same binary with `TEST_SHARD_INDEX` / `TEST_TOTAL_SHARDS` set
//! (and each getting its own `OmniSim` via the per-process singleton in
//! `rp_harness::omnisim`). cucumber has no native support for those env vars,
//! so suites opt in by calling [`scenario_in_current_shard`] from the filter
//! closure they pass to `filter_run_and_exit`:
//!
//! ```rust,ignore
//! .filter_run_and_exit("tests/features", |feat, _rule, sc| {
//!     bdd_infra::sharding::scenario_in_current_shard(
//!         feat.path.as_deref(),
//!         &feat.name,
//!         sc.position.line,
//!     )
//! })
//! ```
//!
//! Scenarios are partitioned by a stable hash of the feature file name and
//! the scenario's line number, so every shard — running the same binary over
//! the same feature tree — computes the same partition, each scenario lands
//! in exactly one shard, and the assignment does not depend on discovery
//! order, environment, or platform. `@serial` semantics are unaffected:
//! cucumber still serializes `@serial` scenarios *within* a shard, and
//! cross-shard isolation comes from each shard process owning its own
//! `OmniSim` instance.
//!
//! A target that sets `shard_count` without wiring its filter through
//! [`scenario_in_current_shard`] still passes — every shard just runs the
//! full suite, multiplying the work `shard_count` times. Don't do that.

use std::path::Path;
use std::sync::OnceLock;

/// Shard assignment parsed from Bazel's test env, cached for the process
/// lifetime. `None` means "not sharded — run everything".
static SHARD: OnceLock<Option<(u64, u64)>> = OnceLock::new();

/// Touch `TEST_SHARD_STATUS_FILE` if Bazel provided one.
///
/// Bazel requires a test runner to create this file to advertise that it
/// understands `TEST_SHARD_INDEX` / `TEST_TOTAL_SHARDS`; with
/// `shard_count > 1` and no status file, Bazel fails the test outright
/// ("test runner did not advertise sharding support"). `bdd_main!` calls
/// this unconditionally — it is a no-op outside Bazel and for unsharded
/// targets.
///
/// Panics if the touch fails: Bazel would fail the shard anyway, but only
/// *after* the process has run its full scenario slice, and with an error
/// pointing at sharding support rather than at the actual I/O problem.
/// Failing here surfaces the root cause before any scenario runs.
pub fn advertise_bazel_sharding_support() {
    if let Some(path) = std::env::var_os("TEST_SHARD_STATUS_FILE") {
        std::fs::write(&path, b"").unwrap_or_else(|e| {
            panic!(
                "bdd_main: failed to touch TEST_SHARD_STATUS_FILE {path:?}: {e} — \
                 Bazel would treat the runner as non-sharding-aware and fail the \
                 test after running the whole suite"
            )
        });
    }
}

/// Whether the given scenario belongs to this process's shard.
///
/// Outside Bazel sharding (no/invalid `TEST_SHARD_INDEX` /
/// `TEST_TOTAL_SHARDS`, or a total of 1) every scenario belongs — the suite
/// behaves exactly as before. Pass the feature's `path`, its `name` (used
/// only when the parser supplied no path), and the scenario's `position.line`.
///
/// Takes primitives instead of `gherkin` types so the base `bdd-infra`
/// crate doesn't grow a cucumber dependency.
#[must_use]
pub fn scenario_in_current_shard(
    feature_path: Option<&Path>,
    feature_name: &str,
    scenario_line: usize,
) -> bool {
    match current_shard() {
        None => true,
        Some((index, total)) => {
            shard_for_scenario(feature_path, feature_name, scenario_line, total) == index
        }
    }
}

/// Read and cache the shard assignment from the Bazel test env.
fn current_shard() -> Option<(u64, u64)> {
    *SHARD.get_or_init(|| {
        parse_shard_env(
            std::env::var("TEST_SHARD_INDEX").ok().as_deref(),
            std::env::var("TEST_TOTAL_SHARDS").ok().as_deref(),
        )
    })
}

/// Parse `(TEST_SHARD_INDEX, TEST_TOTAL_SHARDS)` into a shard assignment.
///
/// Anything malformed — missing vars, non-numeric values, a total of 0 or 1,
/// an index out of range — degrades to `None` (run all scenarios). Degrading
/// to "run everything" can only duplicate work across shards, never lose
/// scenarios.
fn parse_shard_env(index: Option<&str>, total: Option<&str>) -> Option<(u64, u64)> {
    let index = index?.parse::<u64>().ok()?;
    let total = total?.parse::<u64>().ok()?;
    (total > 1 && index < total).then_some((index, total))
}

/// Deterministically assign a scenario to a shard in `0..total`.
///
/// Keys on the feature's file *name* (not its full path — absolute prefixes
/// differ between the cargo and Bazel runfiles trees) plus the scenario's
/// line number, hashed with FNV-1a. FNV-1a is fully specified, so the
/// partition is stable across processes, platforms, and toolchain versions —
/// `std`'s `DefaultHasher` promises none of that.
fn shard_for_scenario(
    feature_path: Option<&Path>,
    feature_name: &str,
    scenario_line: usize,
    total: u64,
) -> u64 {
    let file_name = feature_path.and_then(Path::file_name).map_or_else(
        || feature_name.to_string(),
        |n| n.to_string_lossy().into_owned(),
    );
    let key = format!("{file_name}:{scenario_line}");
    // A zero shard count is a caller bug; land everything in shard 0
    // (over-running a shard) rather than panicking the harness.
    fnv1a(key.as_bytes()).checked_rem(total).unwrap_or(0)
}

/// 64-bit FNV-1a. Tiny, dependency-free, and stable by specification.
fn fnv1a(bytes: &[u8]) -> u64 {
    const OFFSET_BASIS: u64 = 0xcbf2_9ce4_8422_2325;
    const PRIME: u64 = 0x0000_0100_0000_01b3;
    let mut hash = OFFSET_BASIS;
    for &b in bytes {
        hash ^= u64::from(b);
        hash = hash.wrapping_mul(PRIME);
    }
    hash
}

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
mod tests {
    use super::*;

    #[test]
    fn fnv1a_matches_reference_vectors() {
        // Published FNV-1a 64-bit test vectors.
        assert_eq!(fnv1a(b""), 0xcbf2_9ce4_8422_2325);
        assert_eq!(fnv1a(b"a"), 0xaf63_dc4c_8601_ec8c);
        assert_eq!(fnv1a(b"foobar"), 0x8594_4171_f739_67e8);
    }

    #[test]
    fn parse_shard_env_accepts_valid_assignment() {
        assert_eq!(parse_shard_env(Some("2"), Some("8")), Some((2, 8)));
        assert_eq!(parse_shard_env(Some("0"), Some("2")), Some((0, 2)));
    }

    #[test]
    fn parse_shard_env_degrades_to_unsharded_on_junk() {
        assert_eq!(parse_shard_env(None, None), None);
        assert_eq!(parse_shard_env(Some("0"), None), None);
        assert_eq!(parse_shard_env(None, Some("4")), None);
        assert_eq!(parse_shard_env(Some("x"), Some("4")), None);
        assert_eq!(parse_shard_env(Some("0"), Some("x")), None);
        // A total of 1 is "not sharded".
        assert_eq!(parse_shard_env(Some("0"), Some("1")), None);
        // Index out of range: run everything rather than lose scenarios.
        assert_eq!(parse_shard_env(Some("8"), Some("8")), None);
    }

    #[test]
    fn shard_assignment_is_in_range_and_deterministic() {
        let total = 8;
        let path = Path::new("tests/features/some.feature");
        for line in 1..500usize {
            let owner = shard_for_scenario(Some(path), "Some feature", line, total);
            assert!(owner < total);
            // Same inputs => same shard, every time. Combined with `owner`
            // being a single value in 0..total, each scenario is claimed by
            // exactly one shard across the shard processes.
            assert_eq!(
                owner,
                shard_for_scenario(Some(path), "Some feature", line, total)
            );
        }
    }

    #[test]
    fn partition_is_deterministic_and_path_prefix_independent() {
        // The same file name under different directory prefixes (cargo cwd
        // vs Bazel runfiles) must land in the same shard.
        let a = shard_for_scenario(
            Some(Path::new("tests/features/mount.feature")),
            "Mount",
            42,
            8,
        );
        let b = shard_for_scenario(
            Some(Path::new(
                "/runfiles/_main/services/rp/tests/features/mount.feature",
            )),
            "Mount",
            42,
            8,
        );
        assert_eq!(a, b);
    }

    #[test]
    fn partition_spreads_across_shards() {
        // Not a distribution-quality test — just a regression guard that the
        // hash doesn't collapse the rp-sized suite onto one shard.
        let total = 8;
        let mut seen = std::collections::HashSet::new();
        for line in (5..250usize).step_by(7) {
            seen.insert(shard_for_scenario(
                Some(Path::new("mount.feature")),
                "Mount",
                line,
                total,
            ));
        }
        assert!(
            seen.len() >= 4,
            "expected the partition to use several shards, got {seen:?}"
        );
    }

    #[test]
    fn missing_path_falls_back_to_feature_name() {
        let with_name_a = shard_for_scenario(None, "Feature A", 10, 8);
        let with_name_b = shard_for_scenario(None, "Feature B", 10, 8);
        // Both are valid shard indices; determinism is per name.
        assert_eq!(with_name_a, shard_for_scenario(None, "Feature A", 10, 8));
        assert!(with_name_a < 8 && with_name_b < 8);
    }

    /// Serializes the tests that mutate the process-wide
    /// `TEST_SHARD_STATUS_FILE` env var. Recovers from poisoning so a
    /// panicking sibling (the should-fail test below uses `catch_unwind`,
    /// but belt-and-braces) can't cascade.
    static STATUS_FILE_ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    fn lock_status_env() -> std::sync::MutexGuard<'static, ()> {
        STATUS_FILE_ENV_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    #[test]
    fn advertise_touches_status_file_when_env_set() {
        let _lock = lock_status_env();
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("shard_status");
        std::env::set_var("TEST_SHARD_STATUS_FILE", &path);
        advertise_bazel_sharding_support();
        std::env::remove_var("TEST_SHARD_STATUS_FILE");
        assert!(path.exists(), "status file must be created");
    }

    #[test]
    fn advertise_panics_when_the_status_file_cannot_be_written() {
        let _lock = lock_status_env();
        let dir = tempfile::tempdir().unwrap();
        // A path inside a directory that doesn't exist: the write must fail,
        // and the failure must surface as a panic naming the file — not as a
        // log line followed by Bazel failing the shard after the fact.
        let path = dir.path().join("no-such-dir").join("shard_status");
        std::env::set_var("TEST_SHARD_STATUS_FILE", &path);
        let result = std::panic::catch_unwind(advertise_bazel_sharding_support);
        std::env::remove_var("TEST_SHARD_STATUS_FILE");
        assert!(
            result.is_err(),
            "an unwritable status file must fail loudly"
        );
    }
}
