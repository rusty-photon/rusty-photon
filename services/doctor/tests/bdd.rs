//! BDD entry point for doctor. The suite drives the real binary (built
//! with the `mock` feature) through `--platform-facts`, so every scenario
//! stages its own host state and config directory hermetically. The
//! `@pebble` scenarios additionally spawn a private Pebble ACME directory
//! and run only when `PEBBLE_PATH` and `PEBBLE_CHALLTESTSRV_PATH` are set
//! (docs/skills/testing.md §5.6) — a skip is announced loudly, never
//! silent.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::unreachable,
    clippy::panic,
    clippy::arithmetic_side_effects,
    clippy::as_conversions,
    clippy::indexing_slicing
)]

#[path = "bdd/world.rs"]
mod world;

#[path = "bdd/pebble.rs"]
mod pebble;

#[path = "bdd/steps/mod.rs"]
mod steps;

/// How many scenarios the `@pebble` skip drops, counted from the feature
/// sources (each scenario carries its own `@pebble` line).
fn pebble_scenario_count(features_dir: &str) -> usize {
    let mut count = 0;
    for entry in std::fs::read_dir(features_dir)
        .expect("features dir")
        .flatten()
    {
        if let Ok(content) = std::fs::read_to_string(entry.path()) {
            count += content
                .lines()
                .filter(|line| line.trim() == "@pebble")
                .count();
        }
    }
    count
}

bdd_infra::bdd_main! {
    use cucumber::World as _;
    use world::DoctorWorld;

    let pebble_available = pebble::env_paths().is_some();
    if !pebble_available {
        let dropped = pebble_scenario_count("tests/features");
        // A run that declares itself a CI run has no business skipping the
        // only unmocked coverage of the ACME network path: there the missing
        // locators mean the provisioning broke, not that a dev box lacks a
        // binary. `.bazelrc` sets this under --config=ci.
        assert!(
            std::env::var_os("RUSTY_PHOTON_REQUIRE_PEBBLE").is_none(),
            "RUSTY_PHOTON_REQUIRE_PEBBLE is set but PEBBLE_PATH and/or \
             PEBBLE_CHALLTESTSRV_PATH are not — this run would silently drop \
             {dropped} @pebble scenarios. Provision Pebble (see \
             .github/actions/install-pebble, or scripts/install-pebble.sh) \
             or clear RUSTY_PHOTON_REQUIRE_PEBBLE."
        );
        eprintln!(
            "skipping {dropped} @pebble scenarios: PEBBLE_PATH and/or \
             PEBBLE_CHALLTESTSRV_PATH are not set (docs/skills/testing.md \
             section 5.6 — run scripts/install-pebble.sh to get them locally)"
        );
    }

    DoctorWorld::cucumber()
        .filter_run_and_exit("tests/features", move |feat, _rule, sc| {
            let tagged = |tag: &str, at_tag: &str| {
                feat.tags.iter().chain(sc.tags.iter()).any(|t| t == tag || t == at_tag)
            };
            if tagged("wip", "@wip") {
                return false;
            }
            // File ownership is a Unix concept: the checks that judge it
            // report nothing at all on Windows, so the scenarios asserting
            // their rows have nothing to assert there.
            if !cfg!(unix) && tagged("unix", "@unix") {
                return false;
            }
            pebble_available || !tagged("pebble", "@pebble")
        })
        .await;
}
