//! Step definitions for `real_astap_smoke.feature`.
//!
//! `@requires-astap` keeps these from running in PR jobs even after
//! Phase 4 removed `@wip` — the dedicated nightly cross-platform
//! workflow (Phase 6) sets `ASTAP_BINARY` so they fire there only.
//! Phase 4 ships the test bodies; Phase 6 wires the cron + matrix.

use crate::world::PlateSolverWorld;
use bdd_infra::ServiceHandle;
use cucumber::given;
use std::collections::HashMap;
use std::path::PathBuf;

#[given("the wrapper is running with the real ASTAP_BINARY as its solver")]
async fn given_wrapper_with_real_astap(world: &mut PlateSolverWorld) {
    let astap_path = std::env::var("ASTAP_BINARY")
        .map(PathBuf::from)
        .expect("ASTAP_BINARY env var must be set for @requires-astap scenarios");
    let astap_db = std::env::var("ASTAP_DB_DIR")
        .map(PathBuf::from)
        .expect("ASTAP_DB_DIR env var must be set for @requires-astap scenarios");

    let dir = world.temp_dir_path();
    let extra_env: HashMap<String, String> = HashMap::new();
    let cfg_path = write_real_config(&dir, &astap_path, &astap_db, &extra_env);

    world.astap_binary_path = Some(astap_path);
    world.astap_db_directory = Some(astap_db);
    let cfg_str = cfg_path.to_string_lossy().into_owned();
    let handle = ServiceHandle::start(env!("CARGO_PKG_NAME"), &cfg_str).await;
    world.service_handle = Some(handle);
}

#[given("the m101_known.fits fixture is on disk")]
async fn given_m101_fixture(world: &mut PlateSolverWorld) {
    let fixture_src = fixture_path("m101_known.fits");
    let dir = world.temp_dir_path();
    let dst = dir.join("m101_known.fits");
    std::fs::copy(&fixture_src, &dst).expect("copy m101 fixture");
    world.fits_path = Some(dst);
}

#[given("the degenerate_no_stars.fits fixture is on disk")]
async fn given_degenerate_fixture(world: &mut PlateSolverWorld) {
    let fixture_src = fixture_path("degenerate_no_stars.fits");
    let dir = world.temp_dir_path();
    let dst = dir.join("degenerate_no_stars.fits");
    std::fs::copy(&fixture_src, &dst).expect("copy degenerate fixture");
    world.fits_path = Some(dst);
}

fn fixture_path(name: &str) -> PathBuf {
    let manifest =
        std::env::var("CARGO_MANIFEST_DIR").expect("CARGO_MANIFEST_DIR not set; run via cargo");
    PathBuf::from(manifest).join("tests/fixtures").join(name)
}

fn write_real_config(
    dir: &std::path::Path,
    binary: &std::path::Path,
    db: &std::path::Path,
    extra_env: &HashMap<String, String>,
) -> PathBuf {
    let body = serde_json::json!({
        "server": {
            "bind_address": "127.0.0.1",
            "port": 0,
        },
        "astap_binary_path": binary.to_string_lossy(),
        "astap_db_directory": db.to_string_lossy(),
        "astap_extra_env": extra_env,
        // The blind M 101 scenario solves the stripped fixture in
        // ~48 s on Linux (and likely longer on slower matrix legs).
        // The default 120 s max would silently clamp the scenario's
        // per-request timeout — see `services/plate-solver/src/api.rs`
        // for the `.min(state.max_solve_timeout)` call. 300 s gives
        // Windows/macOS room. This is the largest value any scenario
        // configures, so it sets the floor for the shared client's
        // timeout in `PlateSolverWorld::http_client` — raise that too if
        // this grows, or the client preempts a legitimate solve.
        "max_solve_timeout": "300s",
    })
    .to_string();
    let p = dir.join("config.json");
    std::fs::write(&p, body).expect("write config");
    p
}
