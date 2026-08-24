#![cfg(feature = "conformu")]
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

use bdd_infra::{ConformuRun, ServiceHandle};
use tracing_subscriber::{fmt, EnvFilter};

#[tokio::test]
async fn conformu_compliance_tests() -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    // Initialize tracing to capture ConformU detailed output
    let _ = fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| EnvFilter::new("ascom_alpaca::conformu=trace,info")),
        )
        .with_test_writer()
        .try_init();

    // Create test config
    let test_dir = std::env::temp_dir().join("conformu_test");
    std::fs::create_dir_all(&test_dir)?;

    let config_path = test_dir.join("config.json");
    let status_file = test_dir.join("status.txt");

    let config = serde_json::json!({
        "device": {
            "name": "ConformU Test Monitor",
            "unique_id": "conformu-test-001",
            "description": "Test SafetyMonitor for ConformU compliance"
        },
        "file": {
            "path": status_file,
            "polling_interval": "1s"
        },
        "parsing": {
            "rules": [
                {
                    "type": "contains",
                    "pattern": "SAFE",
                    "safe": true
                }
            ],
            "case_sensitive": false
        },
        "server": {
            "port": 0
        }
    });

    std::fs::write(&config_path, serde_json::to_string_pretty(&config)?)?;
    std::fs::write(&status_file, "SAFE")?;

    let mut handle = ServiceHandle::try_start(
        env!("CARGO_PKG_NAME"),
        config_path
            .to_str()
            .expect("conformu temp path must be UTF-8"),
    )
    .await?;

    println!("::group::ConformU Compliance Test Results");
    println!(
        "Running ASCOM Alpaca compliance tests on port {}...",
        handle.port
    );

    let result = bdd_infra::run_conformu("safetymonitor", &handle.base_url, 0, None).await;

    handle.stop().await;
    std::fs::remove_dir_all(&test_dir).ok();

    match result? {
        ConformuRun::Skipped => {
            println!("CONFORMU_PATH not set; skipped");
        }
        ConformuRun::Passed => {
            println!("ConformU compliance tests PASSED");
            println!("All ASCOM Alpaca compliance requirements met");
        }
    }

    println!("::endgroup::");

    Ok(())
}
