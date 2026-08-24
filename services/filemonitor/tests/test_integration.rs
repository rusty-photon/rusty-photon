//! Integration tests for the filemonitor service.
//!
//! Combines server startup, run-loop reload/stop signals, and CLI argument
//! handling into a single test binary so cargo only links one integration
//! target. Coverage of invalid-configuration rejection lives in the BDD
//! scenario *Reject invalid configuration sources* in
//! `tests/features/configuration.feature`.

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

#[cfg(not(miri))]
use filemonitor::{AlpacaServerConfig, Config, DeviceConfig, FileConfig, ParsingConfig};
#[cfg(not(miri))]
use std::path::PathBuf;

#[tokio::test]
#[cfg(not(miri))]
async fn test_start_server_creation() {
    use filemonitor::start_server;
    use std::time::Duration;
    use tokio::time::timeout;

    let config = Config {
        device: DeviceConfig {
            name: "Test Server".to_string(),
            unique_id: "test-server-001".to_string(),
            description: "Test server device".to_string(),
        },
        file: FileConfig {
            path: PathBuf::from("test_server_file.txt"),
            polling_interval: Duration::from_secs(1),
        },
        parsing: ParsingConfig {
            rules: vec![],
            case_sensitive: false,
        },
        server: AlpacaServerConfig::new(0),
    };

    std::fs::write(&config.file.path, "test").unwrap();

    let server_future = start_server(config.clone(), std::future::pending::<()>());
    let result = timeout(Duration::from_millis(100), server_future).await;

    std::fs::remove_file(&config.file.path).unwrap();

    // We expect timeout since server.start() would block indefinitely
    assert!(result.is_err());
}

// Stop-signal and reload-signal exercises share most of the run-loop scaffolding
// (config, monitor file, shutdown future). Bundling them into one test keeps the
// shared setup in one place rather than duplicating it across two tests.
// Discovery is disabled in both halves (`discovery_port: null`), so this could be
// split into two `#[tokio::test]` functions if a future change makes that useful.
#[tokio::test(flavor = "multi_thread")]
#[cfg(not(miri))]
async fn test_server_loop_stop_and_reload() {
    use filemonitor::run_server_loop;
    use rusty_photon_service_lifecycle::ReloadSignal;
    use std::io::Write;
    use tokio_util::sync::CancellationToken;

    // --- Part 1: stop signal ---
    {
        let dir = tempfile::tempdir().unwrap();
        let config_path = dir.path().join("config.json");
        let monitor_file = dir.path().join("monitor.txt");

        std::fs::write(&monitor_file, "SAFE").unwrap();

        let config = serde_json::json!({
            "device": {
                "name": "Test",
                "unique_id": "test-stop-001",
                "description": "Test device"
            },
            "file": {
                "path": monitor_file.to_str().unwrap(),
                "polling_interval": "60s"
            },
            "parsing": {
                "rules": [],
                "case_sensitive": false
            },
            "server": {
                "port": 0,
                "discovery_port": null
            }
        });

        let mut f = std::fs::File::create(&config_path).unwrap();
        f.write_all(config.to_string().as_bytes()).unwrap();

        let shutdown = CancellationToken::new();
        let reload = ReloadSignal::new();

        let shutdown_trigger = shutdown.clone();
        tokio::spawn(async move {
            tokio::time::sleep(std::time::Duration::from_millis(100)).await;
            shutdown_trigger.cancel();
        });

        let result = run_server_loop(&config_path, shutdown, reload).await;

        assert!(result.is_ok(), "stop test failed: {:?}", result.err());
    }

    // --- Part 2: reload signal ---
    {
        let dir = tempfile::tempdir().unwrap();
        let config_path = dir.path().join("config.json");
        let monitor_file = dir.path().join("monitor.txt");

        std::fs::write(&monitor_file, "SAFE").unwrap();

        let config = serde_json::json!({
            "device": {
                "name": "Test",
                "unique_id": "test-reload-001",
                "description": "Test device"
            },
            "file": {
                "path": monitor_file.to_str().unwrap(),
                "polling_interval": "60s"
            },
            "parsing": {
                "rules": [],
                "case_sensitive": false
            },
            "server": {
                "port": 0,
                "discovery_port": null
            }
        });

        let mut f = std::fs::File::create(&config_path).unwrap();
        f.write_all(config.to_string().as_bytes()).unwrap();

        let shutdown = CancellationToken::new();
        let reload = ReloadSignal::new();

        // Strategy for observing a real reload: corrupt the config file
        // *before* firing the reload signal. If `run_server_loop` honours
        // the reload, it drops the running server, re-enters the loop,
        // calls `load_config(config_path)`, sees malformed JSON, and
        // returns Err — which we assert below. If reload were silently
        // ignored, the original server would keep running until the
        // safety `shutdown.cancel()` fires and the test would get Ok.
        let config_path_for_task = config_path.clone();
        let reload_trigger = reload.clone();
        let shutdown_trigger = shutdown.clone();
        tokio::spawn(async move {
            tokio::time::sleep(std::time::Duration::from_millis(100)).await;
            std::fs::write(&config_path_for_task, "this is not valid json").unwrap();
            reload_trigger.notify();
            // Safety net: if the assertion below is going to fail, make
            // sure we don't hang the test runner.
            tokio::time::sleep(std::time::Duration::from_millis(300)).await;
            shutdown_trigger.cancel();
        });

        let result = run_server_loop(&config_path, shutdown, reload).await;

        assert!(
            result.is_err(),
            "reload should have caused a second load_config that fails on \
             the corrupted file; got Ok which means reload was silently ignored"
        );
    }
}

#[test]
#[cfg(not(miri))] // Skip under miri - process spawning not supported
fn test_cli_help() {
    // Skip under sanitizers due to proc-macro compilation issues
    if std::env::var("RUSTFLAGS")
        .unwrap_or_default()
        .contains("sanitizer")
    {
        return;
    }
    let output = bdd_infra::run_once("filemonitor", &["--help"], None);

    assert!(
        output.status.success(),
        "Command failed with stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("ASCOM Alpaca SafetyMonitor"));
    assert!(stdout.contains("--config"));
    assert!(stdout.contains("--log-level"));
}

/// Assert that a config-load failure reached main (rendered by the runner's
/// error report, ADR-011) and that clap did *not* reject the arguments —
/// i.e., `--log-level <variant>` parsed successfully and the binary then
/// failed opening the missing config.
#[cfg(not(miri))]
fn assert_config_not_found(output: &std::process::Output) {
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(!output.status.success(), "expected non-zero exit");
    assert!(
        !stderr.contains("error: invalid value") && !stderr.contains("error: unexpected argument"),
        "clap rejected the arguments; stderr:\n{stderr}"
    );
    // The report prints the io::Error's Display form; the "file not found"
    // OS code is 2 on both Unix (ENOENT) and Windows (ERROR_FILE_NOT_FOUND),
    // while the message text differs per OS.
    assert!(
        stderr.contains("(os error 2)"),
        "expected config-not-found error; stderr:\n{stderr}"
    );
}

#[test]
#[cfg(not(miri))] // Skip under miri - process spawning not supported
fn test_cli_log_level_flag_accepted() {
    // Skip under sanitizers due to proc-macro compilation issues
    if std::env::var("RUSTFLAGS")
        .unwrap_or_default()
        .contains("sanitizer")
    {
        return;
    }
    // Pass --log-level alongside a nonexistent config; the binary should
    // accept the flag (clap parse OK) and then fail opening the config.
    let output = bdd_infra::run_once(
        "filemonitor",
        &["--config", "nonexistent.json", "--log-level", "debug"],
        None,
    );

    assert_config_not_found(&output);
}

#[test]
#[cfg(not(miri))] // Skip under miri - process spawning not supported
fn test_cli_different_log_levels() {
    // Skip under sanitizers due to proc-macro compilation issues
    if std::env::var("RUSTFLAGS")
        .unwrap_or_default()
        .contains("sanitizer")
    {
        return;
    }
    // Verify clap accepts each tracing Level variant. The nonexistent config
    // makes the binary fail fast after argument parsing.
    for log_level in &["error", "warn", "info", "debug", "trace"] {
        let output = bdd_infra::run_once(
            "filemonitor",
            &["--config", "nonexistent.json", "--log-level", log_level],
            None,
        );

        assert_config_not_found(&output);
    }
}
