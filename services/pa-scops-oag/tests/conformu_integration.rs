//! `ConformU` compliance tests for the Pegasus Scops OAG driver
//!
//! These tests verify ASCOM Alpaca compliance by running the `ConformU` test suite
//! against the driver running in mock mode.
#![cfg(feature = "conformu")]
#![allow(clippy::await_holding_lock)]
#![allow(clippy::unwrap_used, clippy::expect_used)]
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

use bdd_infra::ServiceHandle;
use bdd_infra::{run_conformu, ConformuRun, FullRunSettings};
use std::sync::Mutex;
use tracing_subscriber::{fmt, EnvFilter};

static CONFORMU_LOCK: Mutex<()> = Mutex::new(());

#[tokio::test]
async fn conformu_compliance_tests() -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let _lock = CONFORMU_LOCK.lock().unwrap();

    let _ = fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| EnvFilter::new("ascom_alpaca::conformu=trace,info")),
        )
        .with_test_writer()
        .try_init();

    let test_dir = bdd_infra::scratch::new_dir("conformu-pa-scops-oag-")?;

    let config_path = test_dir.path().join("config.json");
    // ConformU's full test set runs (its URL verbs call SetFullTest()); the
    // settings shape only the run's timeouts. The mock focuser settles
    // instantly, so the default 60 s focuser timeout is longer than needed.
    let conformu_settings = FullRunSettings {
        focuser_timeout_s: 30,
        ..FullRunSettings::default()
    };

    let config = serde_json::json!({
        "serial": {
            "port": "/dev/mock",
            "baud_rate": 19200,
            "polling_interval": "60s",
            "timeout": "2s"
        },
        "server": {
            "port": 0
        },
        "focuser": {
            "enabled": true,
            "name": "ConformU Test Scops OAG",
            "unique_id": "conformu-pa-scops-oag-001",
            "description": "Test Pegasus Scops OAG for ConformU compliance",
            "max_step": 22000
        }
    });

    std::fs::write(&config_path, serde_json::to_string_pretty(&config)?)?;

    // Pre-built pa-scops-oag binary must include the `mock` feature (CI builds
    // with --all-features); the binary is launched with the mock serial port
    // driving /dev/mock from the config.
    let mut handle = ServiceHandle::try_start(
        env!("CARGO_PKG_NAME"),
        config_path
            .to_str()
            .expect("conformu temp path must be UTF-8"),
    )
    .await?;

    println!("::group::ConformU Focuser Compliance Test Results");
    println!(
        "Running ASCOM Alpaca Focuser compliance tests on port {}...",
        handle.port
    );

    // Capture both builder-construction and run-time errors so `handle.stop()`
    // below is unconditional and the service gets a graceful SIGTERM with a
    // chance to flush coverage data.
    let result: Result<(), Box<dyn std::error::Error + Send + Sync>> = async {
        match run_conformu("focuser", &handle.base_url, 0, Some(&conformu_settings)).await? {
            ConformuRun::Skipped => {
                println!("CONFORMU_PATH not set; skipped");
            }
            ConformuRun::Passed => {
                println!("ConformU compliance tests PASSED");
                println!("All ASCOM Alpaca Focuser compliance requirements met");
            }
        }
        Ok(())
    }
    .await;

    match &result {
        Ok(()) => {}
        Err(e) => {
            println!("ConformU compliance tests FAILED");
            println!("Error: {e}");
        }
    }

    println!("::endgroup::");

    handle.stop().await;

    result?;
    Ok(())
}
