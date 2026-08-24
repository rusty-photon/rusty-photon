//! `ConformU` compliance test for the svbony-camera Camera driver.
//!
//! Launches the production binary (built with `--features conformu`, which pulls
//! in the `simulation` backend so the SDK yields one `SV605CC-Simulated` camera)
//! and runs the official ASCOM `ConformU` validator against it.
//!
//! Gated behind the `conformu` feature. When `CONFORMU_PATH` is unset the run is
//! `Skipped` (so the test passes without `ConformU` installed); CI sets it.
#![cfg(feature = "conformu")]
#![allow(clippy::unwrap_used, clippy::expect_used)]
// The serialization mutex is intentionally held across the ConformU awaits.
#![allow(clippy::await_holding_lock)]
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

use std::sync::Mutex;

use bdd_infra::{ConformuRun, ServiceHandle};
use tempfile::TempDir;

/// Serialize `ConformU` runs (each binds its own port, but `ConformU` itself and the
/// shared cache directory are global).
static CONFORMU_LOCK: Mutex<()> = Mutex::new(());

#[tokio::test]
async fn conformu_compliance_tests() -> Result<(), Box<dyn std::error::Error>> {
    let _lock = CONFORMU_LOCK.lock().unwrap();
    let _ = tracing_subscriber::fmt::try_init();

    let temp_dir = TempDir::new()?;
    let config_path = temp_dir.path().join("svbony-camera.json");
    // Camera only: SVBony has no other device kind in scope for this service
    // (ADR-014). Port 0 → OS-assigned.
    let config = serde_json::json!({
        "devices": {},
        "server": { "port": 0 },
    });
    std::fs::write(&config_path, serde_json::to_string_pretty(&config)?)?;

    let mut handle = ServiceHandle::try_start(
        env!("CARGO_PKG_NAME"),
        config_path
            .to_str()
            .expect("conformu temp path must be UTF-8"),
    )
    .await
    .map_err(Box::<dyn std::error::Error>::from)?;

    println!("::group::ConformU Compliance Test Results");
    let camera = bdd_infra::run_conformu("camera", &handle.base_url, 0, None)
        .await
        .map_err(|e| Box::<dyn std::error::Error>::from(e.to_string()));
    println!("::endgroup::");

    handle.stop().await;

    match camera? {
        ConformuRun::Skipped => eprintln!("ConformU skipped (CONFORMU_PATH unset)"),
        ConformuRun::Passed => eprintln!("ConformU camera conformance passed"),
    }

    let _ = temp_dir.close();
    Ok(())
}
