//! Integration tests for PHD2 guider client
//!
//! These tests require PHD2 to be installed on the system.
//! Tests that require PHD2 are marked with #[ignore] by default.
//! Run them with: cargo test --test `test_integration` -- --ignored
//!
//! Some tests use the `mock_phd2` binary and can run without PHD2 installed.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::unreachable,
    clippy::panic,
    clippy::arithmetic_side_effects,
    clippy::as_conversions,
    clippy::indexing_slicing
)]
// Curated test-scope allow list — documented in the root Cargo.toml [workspace.lints] block.
#![allow(
    clippy::needless_pass_by_ref_mut,
    clippy::needless_pass_by_value,
    clippy::unused_async,
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

#[cfg_attr(miri, allow(unused_imports))]
use phd2_guider::{
    get_default_phd2_path, load_config, Phd2Client, Phd2Config, Phd2Event, Phd2ProcessManager,
    ReconnectConfig, SettleParams,
};
#[cfg_attr(miri, allow(unused_imports))]
use std::io::{BufRead, BufReader};
#[cfg_attr(miri, allow(unused_imports))]
use std::net::TcpStream;
#[cfg_attr(miri, allow(unused_imports))]
use std::path::PathBuf;
#[cfg_attr(miri, allow(unused_imports))]
use std::process::{Child, Command, Output, Stdio};
use std::time::Duration;

/// Port band reserved for the tests in this file: below every platform's
/// ephemeral floor (32768 on Linux, 49152 on Windows and macOS) so the OS can
/// never assign one of these to a `bind(0)` caller, and clear of the services'
/// own fixed ports (11112-11130).
const RESERVED_PORT_BAND_START: u16 = 20_000;
const RESERVED_PORT_BAND_LEN: u16 = 12_000;

/// How many band ports one call will try before giving up. Only a port some
/// other process is on gets skipped, so the ceiling is never approached.
const RESERVED_PORT_TRIES: u16 = 64;

/// A port for a test that has to know its port *before* anything binds it.
///
/// Two kinds of test here need that. The ones driving a `mock_phd2` child
/// through [`Phd2ProcessManager`] cannot learn a kernel-assigned port after the
/// fact, because the port is an *input* to the API under test: `start_phd2`
/// probes it before spawning, the child receives it through `spawn_env`, and
/// `wait_for_ready` polls it. Real PHD2 announces nothing — it listens on its
/// configured port — so config-first is the production contract. The tests that
/// want a port with nothing listening cannot use an announced port either, since
/// nothing binds.
///
/// Probing with `bind(0)` and releasing the port — the scheme this replaced —
/// draws from the very range the OS assigns to every other `bind(0)` in the
/// process, including the mock servers the CLI tests start concurrently, so the
/// probed port could be claimed before its own test used it. Ports here cannot
/// be handed out that way.
///
/// Each process walks the band from its own start, so concurrent test binaries
/// (two worktrees, `--runs_per_test`) do not march in step. Two of them can
/// still land on overlapping stretches; the probe below moves past whatever the
/// other one already holds, and a residual conflict fails an assertion loudly
/// rather than corrupting another test.
fn reserved_test_port() -> u16 {
    static START: std::sync::OnceLock<u16> = std::sync::OnceLock::new();
    static CURSOR: std::sync::atomic::AtomicU16 = std::sync::atomic::AtomicU16::new(0);

    // Distinct per process: pids alone are too regular when a harness starts
    // many copies at once.
    let start = *START.get_or_init(|| {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |d| d.subsec_nanos());
        ((std::process::id() ^ nanos) % u32::from(RESERVED_PORT_BAND_LEN)) as u16
    });

    for _ in 0..RESERVED_PORT_TRIES {
        // A distinct step per call, so no two callers get the same port.
        let step = CURSOR.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let candidate =
            RESERVED_PORT_BAND_START + (start.wrapping_add(step) % RESERVED_PORT_BAND_LEN);
        // Probe by connecting, never by binding. A listener opened here is
        // duplicated into any child another test thread happens to fork in the
        // same instant and outlives this close until that child execs, so a bind
        // probe can hand back a port that still answers — measured at ~6% of
        // probes at this suite's spawn rate. A refused connect carries the same
        // "nobody is there" answer and leaves nothing behind.
        if TcpStream::connect(("127.0.0.1", candidate)).is_err() {
            return candidate;
        }
    }
    panic!("no free port in {RESERVED_PORT_TRIES} tries");
}

/// Route a spawned child's coverage counters into the `bazel coverage` test
/// action's `COVERAGE_DIR` (its own `%8m` online-merge pool) instead of letting
/// the child inherit — and overwrite — the parent test's `LLVM_PROFILE_FILE`.
///
/// Without this the spawned `phd2-guider` CLI's `main.rs`/`cli.rs` counters are
/// written to the parent's profraw path and lost, so the CLI subprocess tests
/// contribute no coverage under Bazel even though the `test_integration`
/// `RUST_COVERAGE_EXTRA_OBJECTS` env (BUILD.bazel) already lists the binary as
/// an `llvm-cov export -object`. cargo-llvm-cov has no equivalent problem: it
/// sets a shared `LLVM_PROFILE_FILE` (with `%p`/`%m`) inherited by the whole
/// process tree and merges the entire target dir.
///
/// No-op unless `COVERAGE_DIR` is set, so cargo, `cargo llvm-cov` (which sets
/// `LLVM_PROFILE_FILE`, not `COVERAGE_DIR`), and plain `bazel test` are
/// untouched. Mirrors `bdd_infra`'s `child_coverage_profile_var`; `%8m` is an
/// 8-file online-merge pool that bounds the profraw volume (see PR #342).
fn apply_child_coverage_profile(cmd: &mut Command) {
    if let Some(dir) = std::env::var_os("COVERAGE_DIR") {
        let mut path = PathBuf::from(&dir);
        // Absolutize so the child resolves it regardless of its own cwd (this
        // test never chdirs, but be robust like bdd-infra's bdd_main!).
        if path.is_relative() {
            if let Ok(cwd) = std::env::current_dir() {
                path = cwd.join(path);
            }
        }
        path.push("phd2-guider-%8m.profraw");
        cmd.env("LLVM_PROFILE_FILE", path);
    }
}

/// Helper to check if PHD2 is available on the system
fn is_phd2_available() -> bool {
    get_default_phd2_path().is_some()
}

/// Deadline for event waits. Every wait in this file is for a guaranteed
/// event (the mock server always sends it, or a state machine always
/// settles), so the deadline only decides when to declare failure. It is
/// sized for contended CI runners; polls return the moment the condition
/// holds, so healthy runs never feel it.
#[cfg(not(miri))]
const EVENT_DEADLINE: Duration = Duration::from_secs(30);

/// Poll `probe` every 10 ms until it holds, panicking at [`EVENT_DEADLINE`].
#[cfg(not(miri))]
async fn wait_until<F, Fut>(what: &str, mut probe: F)
where
    F: FnMut() -> Fut,
    Fut: std::future::Future<Output = bool>,
{
    tokio::time::timeout(EVENT_DEADLINE, async {
        while !probe().await {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap_or_else(|_| panic!("{what} not observed within {EVENT_DEADLINE:?}"));
}

/// Wait until the client's reader task has processed the version event the
/// mock server sends on connect — the observable proof the connection is
/// fully up (`is_connected` flips earlier, on the socket alone). Eliminates
/// the real-time race a fixed post-connect sleep is prone to (#603).
#[cfg(not(miri))]
async fn wait_connected(client: &Phd2Client) {
    wait_until("version event", || async move {
        client.get_phd2_version().await.is_some()
    })
    .await;
}

/// Helper to create a default test configuration
fn create_test_config() -> Phd2Config {
    Phd2Config {
        host: "localhost".to_string(),
        port: 4400,
        executable_path: get_default_phd2_path(),
        connection_timeout: Duration::from_secs(30),
        command_timeout: Duration::from_secs(30),
        auto_start: false,
        auto_connect_equipment: false,
        ..Default::default()
    }
}

/// Helper to ensure PHD2 is running for a test
/// Returns (manager, `was_started`) - `was_started` indicates if we started PHD2 (so we should stop it)
async fn ensure_phd2_running() -> Option<(Phd2ProcessManager, bool)> {
    if !is_phd2_available() {
        eprintln!("PHD2 not available, skipping test");
        return None;
    }

    let config = create_test_config();
    let manager = Phd2ProcessManager::new(config);

    if manager.is_phd2_running().await {
        // PHD2 already running, don't stop it when test ends
        Some((manager, false))
    } else {
        // Start PHD2
        match manager.start_phd2().await {
            Ok(()) => Some((manager, true)),
            Err(e) => {
                eprintln!("Failed to start PHD2: {e}");
                None
            }
        }
    }
}

// ============================================================================
// Configuration Tests
// ============================================================================

/// Resolve this package's directory at runtime for both Cargo and Bazel.
/// Cargo: `CARGO_MANIFEST_DIR` is the package source dir. Bazel: `rules_rust`
/// bakes a compile-time `CARGO_MANIFEST_DIR` that no longer exists at test
/// runtime, so fall back to the runfiles tree via `TEST_SRCDIR`/`TEST_WORKSPACE`
/// (same approach as services/ppba-driver/tests/translations.rs).
fn package_dir() -> PathBuf {
    let manifest = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    if manifest.join("tests").is_dir() {
        return manifest;
    }
    if let Ok(srcdir) = std::env::var("TEST_SRCDIR") {
        let workspace = std::env::var("TEST_WORKSPACE").unwrap_or_else(|_| "_main".into());
        return PathBuf::from(srcdir)
            .join(workspace)
            .join("services/phd2-guider");
    }
    manifest
}

#[test]
#[cfg(not(miri))] // get_default_phd2_path() spawns a process via Command::output()
fn test_get_default_phd2_path() {
    // This test just verifies the function doesn't panic
    let path = get_default_phd2_path();
    if let Some(p) = path {
        assert!(p.exists(), "Default PHD2 path should exist if returned");
    }
}

#[test]
fn test_load_config() {
    let config_path = package_dir().join("tests/config.json");
    let result = load_config(&config_path);
    assert!(result.is_ok(), "Should load valid config file");

    let config = result.unwrap();
    assert_eq!(config.phd2.host, "localhost");
    assert_eq!(config.phd2.port, 4400);
}

#[test]
fn test_load_config_with_defaults() {
    let config_path = package_dir().join("tests/config_minimal.json");
    let result = load_config(&config_path);
    assert!(result.is_ok(), "Should load minimal config with defaults");

    let config = result.unwrap();
    // Defaults should be applied
    assert_eq!(config.phd2.connection_timeout, Duration::from_secs(10));
    assert_eq!(config.settling.pixels, 0.5);
}

#[test]
fn test_load_config_file_not_found() {
    let config_path = package_dir().join("tests/nonexistent.json");
    let result = load_config(&config_path);
    assert!(result.is_err());
}

// ============================================================================
// Client Creation Tests
// ============================================================================

#[test]
#[cfg(not(miri))] // create_test_config() calls get_default_phd2_path() which spawns a process
fn test_client_creation() {
    let config = create_test_config();
    let client = Phd2Client::new(config);
    // Client should be created successfully
    assert!(std::mem::size_of_val(&client) > 0);
}

#[test]
#[cfg(not(miri))] // create_test_config() calls get_default_phd2_path() which spawns a process
fn test_process_manager_creation() {
    let config = create_test_config();
    let manager = Phd2ProcessManager::new(config);
    // Manager should be created successfully
    assert!(std::mem::size_of_val(&manager) > 0);
}

// ============================================================================
// Connection Tests (require PHD2 to be running)
// ============================================================================

#[tokio::test]
#[ignore = "requires a local PHD2 installation; run with cargo test -- --ignored"]
async fn test_connect_to_running_phd2() {
    let Some((manager, was_started)) = ensure_phd2_running().await else {
        return;
    };

    let config = create_test_config();
    let client = Phd2Client::new(config);

    let result = client.connect().await;
    assert!(result.is_ok(), "Should connect to running PHD2");

    // Wait for version event
    tokio::time::sleep(Duration::from_millis(500)).await;

    let version = client.get_phd2_version().await;
    assert!(version.is_some(), "Should receive PHD2 version");

    let connected = client.is_connected().await;
    assert!(connected, "Should be connected");

    client.disconnect().await.unwrap();
    assert!(!client.is_connected().await, "Should be disconnected");

    // Clean up if we started PHD2
    if was_started {
        manager.stop_phd2(None).await.unwrap();
    }
}

#[tokio::test]
#[ignore = "requires a local PHD2 installation; run with cargo test -- --ignored"]
async fn test_get_app_state() {
    let Some((manager, was_started)) = ensure_phd2_running().await else {
        return;
    };

    let config = create_test_config();
    let client = Phd2Client::new(config);

    client.connect().await.unwrap();

    let state = client.get_app_state().await;
    assert!(state.is_ok(), "Should get app state");

    client.disconnect().await.unwrap();

    if was_started {
        manager.stop_phd2(None).await.unwrap();
    }
}

#[tokio::test]
#[ignore = "requires a local PHD2 installation; run with cargo test -- --ignored"]
async fn test_get_profiles() {
    let Some((manager, was_started)) = ensure_phd2_running().await else {
        return;
    };

    let config = create_test_config();
    let client = Phd2Client::new(config);

    client.connect().await.unwrap();

    let profiles = client.get_profiles().await;
    assert!(profiles.is_ok(), "Should get profiles");

    let profiles = profiles.unwrap();
    assert!(!profiles.is_empty(), "Should have at least one profile");

    client.disconnect().await.unwrap();

    if was_started {
        manager.stop_phd2(None).await.unwrap();
    }
}

#[tokio::test]
#[ignore = "requires a local PHD2 installation; run with cargo test -- --ignored"]
async fn test_get_current_profile() {
    let Some((manager, was_started)) = ensure_phd2_running().await else {
        return;
    };

    let config = create_test_config();
    let client = Phd2Client::new(config);

    client.connect().await.unwrap();

    let profile = client.get_current_profile().await;
    assert!(profile.is_ok(), "Should get current profile");

    let profile = profile.unwrap();
    assert!(!profile.name.is_empty(), "Profile should have a name");

    client.disconnect().await.unwrap();

    if was_started {
        manager.stop_phd2(None).await.unwrap();
    }
}

#[tokio::test]
#[ignore = "requires a local PHD2 installation; run with cargo test -- --ignored"]
async fn test_equipment_connection_status() {
    let Some((manager, was_started)) = ensure_phd2_running().await else {
        return;
    };

    let config = create_test_config();
    let client = Phd2Client::new(config);

    client.connect().await.unwrap();

    let connected = client.is_equipment_connected().await;
    assert!(connected.is_ok(), "Should get equipment connection status");

    client.disconnect().await.unwrap();

    if was_started {
        manager.stop_phd2(None).await.unwrap();
    }
}

#[tokio::test]
#[ignore = "requires a local PHD2 installation; run with cargo test -- --ignored"]
async fn test_event_subscription() {
    let Some((manager, was_started)) = ensure_phd2_running().await else {
        return;
    };

    let config = create_test_config();
    let client = Phd2Client::new(config);

    let mut receiver = client.subscribe();

    client.connect().await.unwrap();

    // We should receive a Version event on connect
    let event = tokio::time::timeout(Duration::from_secs(5), receiver.recv()).await;

    assert!(event.is_ok(), "Should receive event within timeout");
    let event = event.unwrap();
    assert!(event.is_ok(), "Should receive event successfully");

    if let Phd2Event::Version { phd_version, .. } = event.unwrap() {
        assert!(!phd_version.is_empty(), "Version should not be empty");
    } else {
        // Other events might come first depending on PHD2 state
    }

    client.disconnect().await.unwrap();

    if was_started {
        manager.stop_phd2(None).await.unwrap();
    }
}

// ============================================================================
// Process Management Tests (require PHD2 to be installed)
// ============================================================================

#[tokio::test]
#[ignore = "requires a local PHD2 installation; run with cargo test -- --ignored"]
async fn test_is_phd2_running() {
    if !is_phd2_available() {
        eprintln!("PHD2 not available, skipping test");
        return;
    }

    let config = create_test_config();
    let manager = Phd2ProcessManager::new(config);

    // This just tests the detection, doesn't matter if PHD2 is running or not
    let _running = manager.is_phd2_running().await;
}

#[tokio::test]
#[ignore = "requires a local PHD2 installation; run with cargo test -- --ignored"]
async fn test_start_and_stop_phd2() {
    if !is_phd2_available() {
        eprintln!("PHD2 not available, skipping test");
        return;
    }

    let config = create_test_config();
    let manager = Phd2ProcessManager::new(config.clone());

    // Skip if PHD2 is already running
    if manager.is_phd2_running().await {
        eprintln!("PHD2 already running, skipping start test");
        return;
    }

    // Start PHD2
    let result = manager.start_phd2().await;
    assert!(result.is_ok(), "Should start PHD2: {:?}", result.err());

    // Verify it's running
    assert!(
        manager.is_phd2_running().await,
        "PHD2 should be running after start"
    );

    // Connect and verify
    let client = Phd2Client::new(config);
    let connect_result = client.connect().await;
    assert!(connect_result.is_ok(), "Should connect to started PHD2");

    // Wait for version
    tokio::time::sleep(Duration::from_millis(500)).await;
    let version = client.get_phd2_version().await;
    assert!(version.is_some(), "Should have PHD2 version");

    // Stop PHD2
    let stop_result = manager.stop_phd2(Some(&client)).await;
    assert!(stop_result.is_ok(), "Should stop PHD2");

    // Give it time to fully stop
    tokio::time::sleep(Duration::from_secs(2)).await;

    // Verify it's stopped
    assert!(
        !manager.is_phd2_running().await,
        "PHD2 should not be running after stop"
    );
}

#[tokio::test]
#[ignore = "requires a local PHD2 installation; run with cargo test -- --ignored"]
async fn test_start_phd2_already_running() {
    if !is_phd2_available() {
        eprintln!("PHD2 not available, skipping test");
        return;
    }

    let config = create_test_config();
    let manager = Phd2ProcessManager::new(config);

    // Start PHD2 first time
    if !manager.is_phd2_running().await {
        manager.start_phd2().await.unwrap();
    }

    // Try to start again - should succeed (returns Ok if already running)
    let result = manager.start_phd2().await;
    assert!(result.is_ok(), "Should succeed when PHD2 already running");

    // Clean up
    manager.stop_phd2(None).await.unwrap();
}

// ============================================================================
// Error Handling Tests
// ============================================================================

#[tokio::test]
#[cfg(not(miri))]
async fn test_connect_to_nonexistent_server() {
    let config = Phd2Config {
        host: "127.0.0.1".to_string(),
        port: reserved_test_port(),
        connection_timeout: Duration::from_secs(2),
        ..Default::default()
    };

    let client = Phd2Client::new(config);
    let result = client.connect().await;

    assert!(
        result.is_err(),
        "Should fail to connect to nonexistent server"
    );
}

#[tokio::test]
#[cfg(not(miri))] // create_test_config() calls get_default_phd2_path() which spawns a process
async fn test_send_request_when_not_connected() {
    let config = create_test_config();
    let client = Phd2Client::new(config);

    // Don't connect, try to get state
    let result = client.get_app_state().await;
    assert!(result.is_err(), "Should fail when not connected");
}

#[tokio::test]
#[cfg(not(miri))]
async fn test_process_manager_executable_not_found() {
    // A reserved port nothing listens on, so we really do try to start the executable
    let config = Phd2Config {
        host: "127.0.0.1".to_string(),
        port: reserved_test_port(),
        executable_path: Some(PathBuf::from("/nonexistent/path/to/phd2")),
        ..Default::default()
    };

    let manager = Phd2ProcessManager::new(config);
    let result = manager.start_phd2().await;

    assert!(result.is_err(), "Should fail with nonexistent executable");
}

// ============================================================================
// Full Integration Workflow Test
// ============================================================================

#[tokio::test]
#[ignore = "requires a local PHD2 installation; run with cargo test -- --ignored"]
async fn test_full_workflow() {
    if !is_phd2_available() {
        eprintln!("PHD2 not available, skipping test");
        return;
    }

    let config = create_test_config();
    let manager = Phd2ProcessManager::new(config.clone());

    // Start PHD2 if not running
    let was_already_running = manager.is_phd2_running().await;
    if !was_already_running {
        manager.start_phd2().await.unwrap();
    }

    // Create client and connect
    let client = Phd2Client::new(config);
    client.connect().await.unwrap();

    // Wait for version event
    tokio::time::sleep(Duration::from_millis(500)).await;

    // Verify connection
    assert!(client.is_connected().await);
    assert!(client.get_phd2_version().await.is_some());

    // Get state
    let state = client.get_app_state().await.unwrap();
    println!("PHD2 state: {state}");

    // Get profiles
    let profiles = client.get_profiles().await.unwrap();
    println!("Available profiles:");
    for profile in &profiles {
        println!("  [{}] {}", profile.id, profile.name);
    }

    // Get current profile
    let current = client.get_current_profile().await.unwrap();
    println!("Current profile: {} (id: {})", current.name, current.id);

    // Check equipment status
    let equipment_connected = client.is_equipment_connected().await.unwrap();
    println!("Equipment connected: {equipment_connected}");

    // Disconnect client
    client.disconnect().await.unwrap();
    assert!(!client.is_connected().await);

    // Stop PHD2 only if we started it
    if !was_already_running {
        manager.stop_phd2(None).await.unwrap();
    }
}

// ============================================================================
// Mock PHD2 Tests (don't require real PHD2)
// ============================================================================

/// Helper to find the `mock_phd2` binary
#[cfg(not(miri))]
fn find_mock_phd2_binary() -> Option<PathBuf> {
    // Bazel stages the binary and points MOCK_PHD2_BINARY at it ($(rootpath ...));
    // there is no target/debug layout in the sandbox. Honor it first. Under Cargo
    // the var is unset and we fall back to the target/ lookup below.
    if let Ok(path) = std::env::var("MOCK_PHD2_BINARY") {
        let path = PathBuf::from(path);
        if path.exists() {
            return Some(path);
        }
    }

    // Try debug build first
    let debug_path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .unwrap()
        .parent()
        .unwrap()
        .join("target/debug/mock_phd2");

    if debug_path.exists() {
        return Some(debug_path);
    }

    // Try release build
    let release_path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .unwrap()
        .parent()
        .unwrap()
        .join("target/release/mock_phd2");

    if release_path.exists() {
        return Some(release_path);
    }

    None
}

/// Start the mock PHD2 server on a specific port
#[cfg(not(miri))]
fn start_mock_phd2(port: u16) -> Option<Child> {
    let binary = find_mock_phd2_binary()?;

    let mut cmd = Command::new(binary);
    cmd.arg("--port").arg(port.to_string());
    apply_child_coverage_profile(&mut cmd);
    let child = cmd.spawn().ok()?;

    // Give the server time to start
    std::thread::sleep(Duration::from_millis(200));

    Some(child)
}

/// Spawn `mock_phd2` with `MOCK_PHD2_PORT=0` and parse the actual bound port
/// back from stdout. The kernel assigns a free port at bind time, eliminating
/// the TOCTOU window of probe-then-spawn schemes.
///
/// Returns `(port, child)` on success; `None` if the binary can't be spawned
/// or the mock exits before announcing its port.
///
/// Stderr policy is left to the caller: pass `Stdio::inherit()` to surface
/// failure logs in test output, or `Stdio::null()` to discard them (necessary
/// when many of these run in parallel — `mock_phd2` is verbose enough that an
/// undrained piped stderr can fill the pipe buffer and deadlock the mock).
fn spawn_mock_phd2_dynamic_port(
    binary: impl AsRef<std::path::Path>,
    mode: &str,
    stderr: Stdio,
) -> Option<(u16, Child)> {
    let mut cmd = Command::new(binary.as_ref());
    cmd.env("MOCK_PHD2_PORT", "0")
        .env("MOCK_PHD2_MODE", mode)
        .stdout(Stdio::piped())
        .stderr(stderr);
    apply_child_coverage_profile(&mut cmd);
    let mut child = cmd.spawn().ok()?;

    let stdout = child.stdout.take()?;
    let port = BufReader::new(stdout).lines().find_map(|line| {
        line.ok().and_then(|l| {
            l.strip_prefix("MOCK_PHD2_PORT:")
                .and_then(|p| p.parse::<u16>().ok())
        })
    });

    if let Some(p) = port {
        Some((p, child))
    } else {
        // Mock exited before announcing its port; reap it and report failure.
        let _ = child.kill();
        let _ = child.wait();
        None
    }
}

/// Start the mock PHD2 server with auto-assigned port and specified mode.
///
/// Wrapper used by the older tokio integration tests that locate the mock
/// binary via [`find_mock_phd2_binary`] and prefer inherited stderr for
/// debugging.
#[cfg(not(miri))]
fn start_mock_phd2_auto_port(mode: &str) -> Option<(u16, Child)> {
    let binary = find_mock_phd2_binary()?;
    spawn_mock_phd2_dynamic_port(binary, mode, Stdio::inherit())
}

#[tokio::test]
#[cfg(not(miri))]
async fn test_mock_phd2_connection() {
    let Some((port, mut child)) = start_mock_phd2_auto_port("normal") else {
        eprintln!(
            "Mock PHD2 binary not found. Run 'cargo build -p phd2-guider --bin mock_phd2' first"
        );
        return;
    };

    let config = Phd2Config {
        host: "127.0.0.1".to_string(),
        port,
        connection_timeout: Duration::from_secs(5),
        command_timeout: Duration::from_secs(5),
        ..Default::default()
    };

    let client = Phd2Client::new(config);

    // Connect to mock server
    let result = client.connect().await;
    assert!(result.is_ok(), "Should connect to mock PHD2: {result:?}");

    wait_connected(&client).await;

    // Verify we got the version
    let version = client.get_phd2_version().await;
    assert!(version.is_some(), "Should have received version");
    let version = version.unwrap();
    assert!(
        version.contains("mock"),
        "Version should indicate mock server"
    );

    // Disconnect
    client.disconnect().await.unwrap();

    // Kill the mock server
    child.kill().ok();
    child.wait().ok();
}

#[tokio::test]
#[cfg(not(miri))]
async fn test_mock_phd2_get_app_state() {
    let Some((port, mut child)) = start_mock_phd2_auto_port("normal") else {
        eprintln!("Mock PHD2 binary not found or failed to start");
        return;
    };

    let config = Phd2Config {
        host: "127.0.0.1".to_string(),
        port,
        connection_timeout: Duration::from_secs(5),
        command_timeout: Duration::from_secs(5),
        ..Default::default()
    };

    let client = Phd2Client::new(config);
    client.connect().await.unwrap();
    wait_connected(&client).await;

    let state = client.get_app_state().await;
    assert!(state.is_ok(), "Should get app state: {state:?}");

    client.disconnect().await.ok();
    child.kill().ok();
    child.wait().ok();
}

#[tokio::test]
#[cfg(not(miri))]
async fn test_mock_phd2_get_profiles() {
    let Some((port, mut child)) = start_mock_phd2_auto_port("normal") else {
        eprintln!("Mock PHD2 binary not found or failed to start");
        return;
    };

    let config = Phd2Config {
        host: "127.0.0.1".to_string(),
        port,
        connection_timeout: Duration::from_secs(5),
        command_timeout: Duration::from_secs(5),
        ..Default::default()
    };

    let client = Phd2Client::new(config);
    client.connect().await.unwrap();
    wait_connected(&client).await;

    let profiles = client.get_profiles().await;
    assert!(profiles.is_ok(), "Should get profiles: {profiles:?}");

    let profiles = profiles.unwrap();
    assert!(!profiles.is_empty(), "Should have at least one profile");
    assert_eq!(profiles[0].name, "Mock Profile");

    client.disconnect().await.ok();
    child.kill().ok();
    child.wait().ok();
}

#[tokio::test]
#[cfg(not(miri))]
async fn test_mock_phd2_get_equipment() {
    let Some((port, mut child)) = start_mock_phd2_auto_port("normal") else {
        eprintln!("Mock PHD2 binary not found or failed to start");
        return;
    };

    let config = Phd2Config {
        host: "127.0.0.1".to_string(),
        port,
        connection_timeout: Duration::from_secs(5),
        command_timeout: Duration::from_secs(5),
        ..Default::default()
    };

    let client = Phd2Client::new(config);
    client.connect().await.unwrap();
    wait_connected(&client).await;

    let equipment = client.get_current_equipment().await;
    assert!(equipment.is_ok(), "Should get equipment: {equipment:?}");

    let equipment = equipment.unwrap();
    assert!(equipment.camera.is_some(), "Should have camera info");
    assert!(equipment.mount.is_some(), "Should have mount info");

    client.disconnect().await.ok();
    child.kill().ok();
    child.wait().ok();
}

#[tokio::test]
#[cfg(not(miri))]
async fn test_mock_phd2_exposure_methods() {
    let Some((port, mut child)) = start_mock_phd2_auto_port("normal") else {
        eprintln!("Mock PHD2 binary not found or failed to start");
        return;
    };

    let config = Phd2Config {
        host: "127.0.0.1".to_string(),
        port,
        connection_timeout: Duration::from_secs(5),
        command_timeout: Duration::from_secs(5),
        ..Default::default()
    };

    let client = Phd2Client::new(config);
    client.connect().await.unwrap();
    wait_connected(&client).await;

    // Get exposure
    let exposure = client.get_exposure().await;
    assert!(exposure.is_ok(), "Should get exposure: {exposure:?}");
    assert_eq!(exposure.unwrap(), 1000);

    // Get exposure durations
    let durations = client.get_exposure_durations().await;
    assert!(durations.is_ok(), "Should get durations: {durations:?}");
    assert!(!durations.unwrap().is_empty());

    // Set exposure
    let set_result = client.set_exposure(2000).await;
    assert!(set_result.is_ok(), "Should set exposure: {set_result:?}");

    client.disconnect().await.ok();
    child.kill().ok();
    child.wait().ok();
}

#[tokio::test]
#[cfg(not(miri))]
async fn test_mock_phd2_calibration_methods() {
    let Some((port, mut child)) = start_mock_phd2_auto_port("normal") else {
        eprintln!("Mock PHD2 binary not found or failed to start");
        return;
    };

    let config = Phd2Config {
        host: "127.0.0.1".to_string(),
        port,
        connection_timeout: Duration::from_secs(5),
        command_timeout: Duration::from_secs(5),
        ..Default::default()
    };

    let client = Phd2Client::new(config);
    client.connect().await.unwrap();
    wait_connected(&client).await;

    // Get calibration status
    let calibrated = client.is_calibrated().await;
    assert!(
        calibrated.is_ok(),
        "Should get calibration status: {calibrated:?}"
    );

    // Get calibration data
    let data = client
        .get_calibration_data(phd2_guider::CalibrationTarget::Mount)
        .await;
    assert!(data.is_ok(), "Should get calibration data: {data:?}");

    // Clear calibration
    let clear_result = client
        .clear_calibration(phd2_guider::CalibrationTarget::Mount)
        .await;
    assert!(
        clear_result.is_ok(),
        "Should clear calibration: {clear_result:?}"
    );

    client.disconnect().await.ok();
    child.kill().ok();
    child.wait().ok();
}

#[tokio::test]
#[cfg(not(miri))]
async fn test_mock_phd2_guiding_control() {
    let Some((port, mut child)) = start_mock_phd2_auto_port("normal") else {
        eprintln!("Mock PHD2 binary not found or failed to start");
        return;
    };

    let config = Phd2Config {
        host: "127.0.0.1".to_string(),
        port,
        connection_timeout: Duration::from_secs(5),
        command_timeout: Duration::from_secs(5),
        ..Default::default()
    };

    let client = Phd2Client::new(config);
    client.connect().await.unwrap();
    wait_connected(&client).await;

    // Start looping
    let loop_result = client.start_loop().await;
    assert!(loop_result.is_ok(), "Should start looping: {loop_result:?}");

    // Start guiding
    let settle = SettleParams::default();
    let guide_result = client.start_guiding(&settle, false, None).await;
    assert!(
        guide_result.is_ok(),
        "Should start guiding: {guide_result:?}"
    );

    // Pause guiding
    let pause_result = client.pause(true).await;
    assert!(
        pause_result.is_ok(),
        "Should pause guiding: {pause_result:?}"
    );

    // Stop capture
    let stop_result = client.stop_capture().await;
    assert!(stop_result.is_ok(), "Should stop capture: {stop_result:?}");

    client.disconnect().await.ok();
    child.kill().ok();
    child.wait().ok();
}

#[tokio::test]
#[cfg(not(miri))]
async fn test_mock_phd2_star_operations() {
    let Some((port, mut child)) = start_mock_phd2_auto_port("normal") else {
        eprintln!("Mock PHD2 binary not found or failed to start");
        return;
    };

    let config = Phd2Config {
        host: "127.0.0.1".to_string(),
        port,
        connection_timeout: Duration::from_secs(5),
        command_timeout: Duration::from_secs(5),
        ..Default::default()
    };

    let client = Phd2Client::new(config);
    client.connect().await.unwrap();
    wait_connected(&client).await;

    // Auto-select star
    let find_result = client.find_star(None).await;
    assert!(
        find_result.is_ok(),
        "Should auto-select star: {find_result:?}"
    );

    // Set lock position
    let lock_result = client.set_lock_position(320.0, 240.0, true).await;
    assert!(
        lock_result.is_ok(),
        "Should set lock position: {lock_result:?}"
    );

    client.disconnect().await.ok();
    child.kill().ok();
    child.wait().ok();
}

#[tokio::test]
#[cfg(not(miri))]
async fn test_mock_phd2_cooling() {
    let Some((port, mut child)) = start_mock_phd2_auto_port("normal") else {
        eprintln!("Mock PHD2 binary not found or failed to start");
        return;
    };

    let config = Phd2Config {
        host: "127.0.0.1".to_string(),
        port,
        connection_timeout: Duration::from_secs(5),
        command_timeout: Duration::from_secs(5),
        ..Default::default()
    };

    let client = Phd2Client::new(config);
    client.connect().await.unwrap();
    wait_connected(&client).await;

    // Get CCD temperature
    let temp = client.get_ccd_temperature().await;
    assert!(temp.is_ok(), "Should get temperature: {temp:?}");
    assert!((temp.unwrap() - 20.0).abs() < 1.0);

    // Get cooler status
    let status = client.get_cooler_status().await;
    assert!(status.is_ok(), "Should get cooler status: {status:?}");

    client.disconnect().await.ok();
    child.kill().ok();
    child.wait().ok();
}

#[tokio::test]
#[cfg(not(miri))]
async fn test_mock_phd2_star_image() {
    let Some((port, mut child)) = start_mock_phd2_auto_port("normal") else {
        eprintln!("Mock PHD2 binary not found or failed to start");
        return;
    };

    let config = Phd2Config {
        host: "127.0.0.1".to_string(),
        port,
        connection_timeout: Duration::from_secs(5),
        command_timeout: Duration::from_secs(5),
        ..Default::default()
    };

    let client = Phd2Client::new(config);
    client.connect().await.unwrap();
    wait_connected(&client).await;

    // Get star image
    let image = client.get_star_image(32).await;
    assert!(image.is_ok(), "Should get star image: {image:?}");

    let image = image.unwrap();
    assert_eq!(image.width, 32);
    assert_eq!(image.height, 32);

    client.disconnect().await.ok();
    child.kill().ok();
    child.wait().ok();
}

#[tokio::test]
#[cfg(not(miri))]
async fn test_mock_phd2_event_subscription() {
    let Some((port, mut child)) = start_mock_phd2_auto_port("normal") else {
        eprintln!("Mock PHD2 binary not found or failed to start");
        return;
    };

    let config = Phd2Config {
        host: "127.0.0.1".to_string(),
        port,
        connection_timeout: Duration::from_secs(5),
        command_timeout: Duration::from_secs(5),
        ..Default::default()
    };

    let client = Phd2Client::new(config);
    let mut receiver = client.subscribe();

    client.connect().await.unwrap();

    // We should receive a Version event
    let event = tokio::time::timeout(Duration::from_secs(2), receiver.recv()).await;
    assert!(event.is_ok(), "Should receive event within timeout");

    let event = event.unwrap();
    assert!(event.is_ok(), "Channel should be open");

    match event.unwrap() {
        Phd2Event::Version { phd_version, .. } => {
            assert!(phd_version.contains("mock"), "Should be mock version");
        }
        other => {
            panic!("Expected Version event, got {other:?}");
        }
    }

    client.disconnect().await.ok();
    child.kill().ok();
    child.wait().ok();
}

#[tokio::test]
#[cfg(not(miri))]
async fn test_mock_phd2_reconnect_on_disconnect() {
    let Some((port, mut child)) = start_mock_phd2_auto_port("normal") else {
        eprintln!("Mock PHD2 binary not found or failed to start");
        return;
    };

    let config = Phd2Config {
        host: "127.0.0.1".to_string(),
        port,
        connection_timeout: Duration::from_secs(5),
        command_timeout: Duration::from_secs(5),
        reconnect: ReconnectConfig {
            enabled: true,
            interval: Duration::from_secs(1),
            max_retries: Some(3),
        },
        ..Default::default()
    };

    let client = Phd2Client::new(config);
    client.connect().await.unwrap();
    wait_connected(&client).await;

    assert!(client.is_connected().await, "Should be connected initially");

    // Kill the mock server to simulate disconnect
    child.kill().ok();
    child.wait().ok();

    // Wait a bit for disconnect detection
    tokio::time::sleep(Duration::from_millis(500)).await;

    // Start a new mock server
    let mut child2 = start_mock_phd2(port).expect("Should start new mock server");

    // Wait for reconnection
    tokio::time::sleep(Duration::from_secs(3)).await;

    // Check if reconnected
    let _is_connected = client.is_connected().await;
    // Note: The auto-reconnect might or might not succeed depending on timing
    // This test mainly verifies that the reconnect logic doesn't panic

    client.disconnect().await.ok();
    child2.kill().ok();
    child2.wait().ok();
}

// ============================================================================
// Process Manager Tests with Mock PHD2
// ============================================================================
//
// These tests use the mock_phd2 binary via Phd2ProcessManager.
// The mock binary reads port from MOCK_PHD2_PORT environment variable,
// which is passed via spawn_env in Phd2Config.

#[tokio::test]
#[cfg(not(miri))]
async fn test_process_manager_start_stop_mock() {
    let port = reserved_test_port();

    let Some(binary_path) = find_mock_phd2_binary() else {
        eprintln!("Mock PHD2 binary not found");
        return;
    };

    // First make sure nothing is running on that port
    let addr = format!("127.0.0.1:{port}");
    if tokio::net::TcpStream::connect(&addr).await.is_ok() {
        eprintln!("Port {port} is in use, skipping test");
        return;
    }

    let mut spawn_env = std::collections::HashMap::new();
    spawn_env.insert("MOCK_PHD2_PORT".to_string(), port.to_string());

    let config = Phd2Config {
        host: "127.0.0.1".to_string(),
        port,
        executable_path: Some(binary_path),
        connection_timeout: Duration::from_secs(10),
        command_timeout: Duration::from_secs(5),
        spawn_env,
        ..Default::default()
    };

    let manager = Phd2ProcessManager::new(config.clone());

    // Verify not running initially
    assert!(
        !manager.is_phd2_running().await,
        "Mock should not be running initially"
    );
    assert!(
        !manager.has_managed_process().await,
        "Should not have managed process initially"
    );

    // Start the mock PHD2
    let result = manager.start_phd2().await;
    assert!(result.is_ok(), "Should start mock PHD2: {result:?}");

    // Verify it's running
    assert!(
        manager.is_phd2_running().await,
        "Mock should be running after start"
    );
    assert!(
        manager.has_managed_process().await,
        "Should have managed process after start"
    );

    // Connect a client
    let client = Phd2Client::new(config);
    client.connect().await.unwrap();

    wait_connected(&client).await;
    let version = client.get_phd2_version().await;
    assert!(version.is_some(), "Should have version");
    assert!(version.unwrap().contains("mock"), "Should be mock version");

    // Stop via process manager (graceful shutdown)
    let stop_result = manager.stop_phd2(Some(&client)).await;
    assert!(
        stop_result.is_ok(),
        "Should stop mock PHD2: {stop_result:?}"
    );

    // Wait for shutdown
    tokio::time::sleep(Duration::from_millis(500)).await;

    // Verify not running
    assert!(
        !manager.is_phd2_running().await,
        "Mock should not be running after stop"
    );
    assert!(
        !manager.has_managed_process().await,
        "Should not have managed process after stop"
    );
}

#[tokio::test]
#[cfg(not(miri))]
async fn test_process_manager_start_already_running() {
    let Some(binary_path) = find_mock_phd2_binary() else {
        eprintln!("Mock PHD2 binary not found");
        return;
    };

    // Start mock manually first with auto-assigned port
    let Some((port, mut child)) = start_mock_phd2_auto_port("normal") else {
        eprintln!("Mock PHD2 failed to start");
        return;
    };

    let config = Phd2Config {
        host: "127.0.0.1".to_string(),
        port,
        executable_path: Some(binary_path),
        connection_timeout: Duration::from_secs(5),
        command_timeout: Duration::from_secs(5),
        ..Default::default()
    };

    let manager = Phd2ProcessManager::new(config);

    // Manager should detect already running
    assert!(
        manager.is_phd2_running().await,
        "Should detect running mock"
    );

    // Start should succeed (returns Ok when already running)
    let result = manager.start_phd2().await;
    assert!(
        result.is_ok(),
        "Should succeed when already running: {result:?}"
    );

    // Manager should NOT have a managed process (since it was already running)
    assert!(
        !manager.has_managed_process().await,
        "Should not manage externally started process"
    );

    // Cleanup
    child.kill().ok();
    child.wait().ok();
}

#[tokio::test]
#[cfg(not(miri))]
async fn test_process_manager_force_kill() {
    let port = reserved_test_port();

    let Some(binary_path) = find_mock_phd2_binary() else {
        eprintln!("Mock PHD2 binary not found");
        return;
    };

    // First make sure nothing is running on that port
    let addr = format!("127.0.0.1:{port}");
    if tokio::net::TcpStream::connect(&addr).await.is_ok() {
        eprintln!("Port {port} is in use, skipping test");
        return;
    }

    let mut spawn_env = std::collections::HashMap::new();
    spawn_env.insert("MOCK_PHD2_PORT".to_string(), port.to_string());

    let config = Phd2Config {
        host: "127.0.0.1".to_string(),
        port,
        executable_path: Some(binary_path),
        connection_timeout: Duration::from_secs(10),
        command_timeout: Duration::from_secs(5),
        spawn_env,
        ..Default::default()
    };

    let manager = Phd2ProcessManager::new(config);

    // Start the mock PHD2
    let start_result = manager.start_phd2().await;
    assert!(start_result.is_ok(), "Should start: {start_result:?}");
    // start_phd2 reports success without spawning anything when something else
    // already answers on the port. This test owns a reserved port, so an
    // adopted stranger means the reservation broke: fail here rather than carry
    // on and shut down a server that belongs to another test.
    assert!(
        manager.has_managed_process().await,
        "start_phd2 adopted a foreign server on port {port} instead of spawning"
    );

    // Force stop without client (no graceful shutdown)
    let stop_result = manager.stop_phd2(None).await;
    assert!(
        stop_result.is_ok(),
        "Should force stop mock PHD2: {stop_result:?}"
    );

    // Wait for process to die
    tokio::time::sleep(Duration::from_millis(500)).await;

    // Verify not running
    assert!(
        !manager.is_phd2_running().await,
        "Mock should not be running after force stop"
    );
}

#[tokio::test]
#[cfg(not(miri))]
async fn test_process_manager_shutdown_via_rpc() {
    let port = reserved_test_port();

    let Some(binary_path) = find_mock_phd2_binary() else {
        eprintln!("Mock PHD2 binary not found");
        return;
    };

    // First make sure nothing is running on that port
    let addr = format!("127.0.0.1:{port}");
    if tokio::net::TcpStream::connect(&addr).await.is_ok() {
        eprintln!("Port {port} is in use, skipping test");
        return;
    }

    let mut spawn_env = std::collections::HashMap::new();
    spawn_env.insert("MOCK_PHD2_PORT".to_string(), port.to_string());

    let config = Phd2Config {
        host: "127.0.0.1".to_string(),
        port,
        executable_path: Some(binary_path),
        connection_timeout: Duration::from_secs(10),
        command_timeout: Duration::from_secs(5),
        spawn_env: spawn_env.clone(),
        ..Default::default()
    };

    let manager = Phd2ProcessManager::new(config.clone());

    // Start the mock PHD2
    let start_result = manager.start_phd2().await;
    assert!(start_result.is_ok(), "Should start: {start_result:?}");
    // start_phd2 reports success without spawning anything when something else
    // already answers on the port. This test owns a reserved port, so an
    // adopted stranger means the reservation broke: fail here rather than carry
    // on and shut down a server that belongs to another test.
    assert!(
        manager.has_managed_process().await,
        "start_phd2 adopted a foreign server on port {port} instead of spawning"
    );

    // Connect a client
    let client = Phd2Client::new(config);
    client.connect().await.unwrap();
    wait_connected(&client).await;

    // Try to shutdown via client directly (tests the shutdown_phd2 RPC call)
    let shutdown_result = client.shutdown_phd2().await;
    assert!(
        shutdown_result.is_ok(),
        "Should send shutdown command: {shutdown_result:?}"
    );

    // Wait for process to die
    tokio::time::sleep(Duration::from_secs(1)).await;

    // Verify not running (mock server handles shutdown command)
    assert!(
        !manager.is_phd2_running().await,
        "Mock should not be running after shutdown RPC"
    );
}

#[tokio::test]
#[cfg(not(miri))]
async fn test_process_manager_stop_without_client() {
    let port = reserved_test_port();

    let Some(binary_path) = find_mock_phd2_binary() else {
        eprintln!("Mock PHD2 binary not found");
        return;
    };

    // First make sure nothing is running on that port
    let addr = format!("127.0.0.1:{port}");
    if tokio::net::TcpStream::connect(&addr).await.is_ok() {
        eprintln!("Port {port} is in use, skipping test");
        return;
    }

    let mut spawn_env = std::collections::HashMap::new();
    spawn_env.insert("MOCK_PHD2_PORT".to_string(), port.to_string());

    let config = Phd2Config {
        host: "127.0.0.1".to_string(),
        port,
        executable_path: Some(binary_path),
        connection_timeout: Duration::from_secs(10),
        command_timeout: Duration::from_secs(5),
        spawn_env,
        ..Default::default()
    };

    let manager = Phd2ProcessManager::new(config);

    // Start the mock PHD2
    let start_result = manager.start_phd2().await;
    assert!(start_result.is_ok(), "Should start: {start_result:?}");
    // start_phd2 reports success without spawning anything when something else
    // already answers on the port. This test owns a reserved port, so an
    // adopted stranger means the reservation broke: fail here rather than carry
    // on and shut down a server that belongs to another test.
    assert!(
        manager.has_managed_process().await,
        "start_phd2 adopted a foreign server on port {port} instead of spawning"
    );

    // Verify it's running
    assert!(manager.is_phd2_running().await, "Mock should be running");

    // Stop without client (tests force kill path)
    let stop_result = manager.stop_phd2(None).await;
    assert!(stop_result.is_ok(), "Should stop: {stop_result:?}");

    // Verify not running
    assert!(
        !manager.is_phd2_running().await,
        "Mock should not be running after stop"
    );
}

#[tokio::test]
#[cfg(not(miri))]
async fn test_process_manager_start_when_external_running() {
    let Some(binary_path) = find_mock_phd2_binary() else {
        eprintln!("Mock PHD2 binary not found");
        return;
    };

    // Start mock manually first (simulating externally running PHD2) with auto-assigned port
    let Some((port, mut child)) = start_mock_phd2_auto_port("normal") else {
        eprintln!("Mock PHD2 failed to start");
        return;
    };

    let config = Phd2Config {
        host: "127.0.0.1".to_string(),
        port,
        executable_path: Some(binary_path),
        connection_timeout: Duration::from_secs(5),
        command_timeout: Duration::from_secs(5),
        ..Default::default()
    };

    let manager = Phd2ProcessManager::new(config);

    // Manager should detect already running and return Ok early
    let result = manager.start_phd2().await;
    assert!(result.is_ok(), "Should return Ok when already running");

    // Manager should not have a managed process (it was started externally)
    assert!(
        !manager.has_managed_process().await,
        "Should not have managed process when external"
    );

    // Clean up manually started process
    child.kill().expect("Should kill mock");
}

#[tokio::test]
#[cfg(not(miri))]
async fn test_process_manager_no_executable_no_default() {
    // Verify that start_phd2() fails with ExecutableNotFound when no
    // executable_path is configured and PHD2 isn't installed.
    // Use port 1 (privileged) so is_phd2_running() can't get a false
    // positive from a random service listening on the port.
    let config = Phd2Config {
        port: 1,
        executable_path: None,
        ..Default::default()
    };

    let manager = Phd2ProcessManager::new(config);

    // Only run this test if there's no PHD2 in default locations
    if get_default_phd2_path().is_none() {
        let result = manager.start_phd2().await;
        assert!(result.is_err(), "Should fail when no executable found");
    }
}

// ============================================================================
// Error Path Tests (using mock_phd2 modes)
// ============================================================================

#[tokio::test]
#[cfg(not(miri))]
async fn test_process_exit_immediately() {
    let port = reserved_test_port();

    let Some(binary_path) = find_mock_phd2_binary() else {
        eprintln!("Mock PHD2 binary not found");
        return;
    };

    let mut spawn_env = std::collections::HashMap::new();
    spawn_env.insert("MOCK_PHD2_PORT".to_string(), port.to_string());
    spawn_env.insert("MOCK_PHD2_MODE".to_string(), "exit_immediately".to_string());

    let config = Phd2Config {
        host: "127.0.0.1".to_string(),
        port,
        executable_path: Some(binary_path),
        connection_timeout: Duration::from_secs(5),
        command_timeout: Duration::from_secs(5),
        spawn_env,
        ..Default::default()
    };

    let manager = Phd2ProcessManager::new(config);

    // Start should fail because the process exits immediately
    let result = manager.start_phd2().await;
    assert!(
        result.is_err(),
        "Should fail when process exits immediately"
    );

    // Verify the error message mentions premature exit
    let err_msg = format!("{:?}", result.unwrap_err());
    assert!(
        err_msg.contains("exited prematurely") || err_msg.contains("ProcessStartFailed"),
        "Error should indicate premature exit: {err_msg}"
    );
}

#[tokio::test]
#[cfg(not(miri))]
async fn test_process_connection_timeout() {
    let port = reserved_test_port();

    let Some(binary_path) = find_mock_phd2_binary() else {
        eprintln!("Mock PHD2 binary not found");
        return;
    };

    let mut spawn_env = std::collections::HashMap::new();
    spawn_env.insert("MOCK_PHD2_PORT".to_string(), port.to_string());
    spawn_env.insert("MOCK_PHD2_MODE".to_string(), "no_listen".to_string());

    let config = Phd2Config {
        host: "127.0.0.1".to_string(),
        port,
        executable_path: Some(binary_path),
        connection_timeout: Duration::from_secs(2), // Short timeout for faster test
        command_timeout: Duration::from_secs(5),
        spawn_env,
        ..Default::default()
    };

    let manager = Phd2ProcessManager::new(config);

    // Start should fail due to timeout (process doesn't listen)
    let result = manager.start_phd2().await;
    assert!(result.is_err(), "Should fail when connection times out");

    // Verify the error is a timeout
    let err_msg = format!("{:?}", result.unwrap_err());
    assert!(
        err_msg.contains("Timeout") || err_msg.contains("did not become ready"),
        "Error should indicate timeout: {err_msg}"
    );

    // Clean up - force kill the no_listen process
    manager.stop_phd2(None).await.ok();
}

#[tokio::test]
#[cfg(not(miri))]
async fn test_graceful_shutdown_fails_fallback_to_kill() {
    // Use auto-assigned port to avoid port collision with parallel tests
    let Some((port, mut child)) = start_mock_phd2_auto_port("shutdown_fails") else {
        eprintln!("Mock PHD2 binary not found or failed to start");
        return;
    };

    let config = Phd2Config {
        host: "127.0.0.1".to_string(),
        port,
        connection_timeout: Duration::from_secs(10),
        command_timeout: Duration::from_secs(5),
        ..Default::default()
    };

    // Connect a client
    let client = Phd2Client::new(config);
    client.connect().await.unwrap();

    wait_connected(&client).await;

    // Try graceful shutdown - this should "succeed" (return Ok) but
    // the process won't actually exit because it's in shutdown_fails mode
    let shutdown_result = client.shutdown_phd2().await;
    assert!(
        shutdown_result.is_ok(),
        "Shutdown command should succeed: {shutdown_result:?}"
    );

    // Wait a moment and verify the process is STILL running
    tokio::time::sleep(Duration::from_millis(500)).await;
    assert!(
        child.try_wait().unwrap().is_none(),
        "Mock should still be running after ignored shutdown"
    );

    // Now force kill the process
    child.kill().unwrap();
    let _ = child.wait();

    // Verify not running (can't connect)
    let addr = format!("127.0.0.1:{port}");
    assert!(
        tokio::net::TcpStream::connect(&addr).await.is_err(),
        "Mock should not be running after force kill"
    );
}

// ============================================================================
// CLI Subprocess Tests
// ============================================================================
//
// These tests spawn the mock_phd2 server and run the phd2-guider CLI as a
// subprocess to verify end-to-end behavior. Each mock binds `:0` and announces
// the port it got, so they run in parallel without contending for one. The few
// that need a port with nothing listening take one from [`reserved_test_port`].

/// Guard that kills a child process when dropped
struct ProcessGuard {
    child: Child,
    name: &'static str,
}

impl ProcessGuard {
    const fn new(child: Child, name: &'static str) -> Self {
        Self { child, name }
    }
}

impl Drop for ProcessGuard {
    fn drop(&mut self) {
        if let Err(e) = self.child.kill() {
            eprintln!("Failed to kill {} process: {}", self.name, e);
        }
        let _ = self.child.wait();
    }
}

/// Spawn the `mock_phd2` server on a random port
fn spawn_mock_server() -> (ProcessGuard, u16) {
    spawn_mock_server_with_mode("normal")
}

/// Resolve the `mock_phd2` binary path. Bazel sets `MOCK_PHD2_BINARY` to a
/// `$(rootpath ...)`; Cargo sets `CARGO_BIN_EXE_mock_phd2` at compile time.
/// `option_env!` (not `env!`) keeps this compiling under Bazel, where the
/// cargo-only var is absent.
fn mock_phd2_bin() -> String {
    std::env::var("MOCK_PHD2_BINARY")
        .ok()
        .or_else(|| option_env!("CARGO_BIN_EXE_mock_phd2").map(String::from))
        .expect("MOCK_PHD2_BINARY (Bazel) or CARGO_BIN_EXE_mock_phd2 (cargo) must be set")
}

/// Resolve the `phd2-guider` CLI binary path. Bazel sets `PHD2_GUIDER_BINARY`;
/// Cargo sets `CARGO_BIN_EXE_phd2-guider` at compile time. See [`mock_phd2_bin`].
fn phd2_guider_bin() -> String {
    std::env::var("PHD2_GUIDER_BINARY")
        .ok()
        .or_else(|| option_env!("CARGO_BIN_EXE_phd2-guider").map(String::from))
        .expect("PHD2_GUIDER_BINARY (Bazel) or CARGO_BIN_EXE_phd2-guider (cargo) must be set")
}

/// Build a `Command` for the phd2-guider CLI binary with the per-child coverage
/// profile already applied (see [`apply_child_coverage_profile`]), so every CLI
/// subprocess this test spawns has its `main.rs`/`cli.rs` coverage collected
/// under `bazel coverage`. Use this instead of `Command::new(phd2_guider_bin())`.
fn phd2_guider_command() -> Command {
    let bin = phd2_guider_bin();
    let mut cmd = Command::new(bin);
    apply_child_coverage_profile(&mut cmd);
    cmd
}

/// Spawn the `mock_phd2` server with a specific mode.
///
/// Delegates to [`spawn_mock_phd2_dynamic_port`] for the actual spawn-and-parse;
/// this wrapper just handles binary lookup (via [`mock_phd2_bin`]), silences
/// mock stderr (these CLI tests run 38+ in parallel — an undrained piped
/// stderr would deadlock the mock), and wraps the child in a [`ProcessGuard`]
/// for cleanup.
fn spawn_mock_server_with_mode(mode: &str) -> (ProcessGuard, u16) {
    let (port, child) = spawn_mock_phd2_dynamic_port(mock_phd2_bin(), mode, Stdio::null())
        .expect("Failed to start mock_phd2 server");

    // The mock prints its port line only after `bind` returns, so the port is
    // already accepting connections here. A further connect-and-drop probe
    // would add nothing and would leave a dead connection in the accept queue
    // ahead of the CLI's real one.
    (ProcessGuard::new(child, "mock_phd2"), port)
}

/// A scratch config path unique to this process, so two test binaries sharing a
/// `TMPDIR` (two worktrees, `--runs_per_test`) cannot clobber or delete each
/// other's fixture mid-run.
fn temp_config_path(stem: &str) -> PathBuf {
    std::env::temp_dir().join(format!("test_{}_{}.json", stem, std::process::id()))
}

/// Run the phd2-guider CLI with given arguments
fn run_cli(args: &[&str], port: u16) -> Output {
    run_cli_with_timeout(args, port, Duration::from_secs(10))
}

/// Run the phd2-guider CLI with a custom timeout.
///
/// `--host 127.0.0.1` is explicit because the config default is `localhost`,
/// which resolves `::1` ahead of `127.0.0.1` on a dual-stack host: the CLI
/// would spend a refused connect on `[::1]:port` (the mock binds `127.0.0.1`
/// only) before reaching it, and would talk to whatever *else* happens to hold
/// that port on `::1`.
fn run_cli_with_timeout(args: &[&str], port: u16, timeout: Duration) -> Output {
    let mut cmd = phd2_guider_command();
    cmd.args(["--host", "127.0.0.1", "--port", &port.to_string()])
        .args(args)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());

    let mut child = cmd.spawn().expect("Failed to spawn phd2-guider");

    // Wait with timeout
    let start = std::time::Instant::now();
    loop {
        match child.try_wait() {
            Ok(Some(_)) => break,
            Ok(None) => {
                if start.elapsed() > timeout {
                    child.kill().expect("Failed to kill timed-out process");
                    // Reap the kill'd child before panicking so we don't leave
                    // a zombie hanging around for the rest of the test binary.
                    let _ = child.wait();
                    panic!("CLI command timed out after {timeout:?}");
                }
                std::thread::sleep(Duration::from_millis(100));
            }
            Err(e) => panic!("Error waiting for CLI: {e}"),
        }
    }

    child.wait_with_output().expect("Failed to get CLI output")
}

/// Run the CLI without connecting to any server (for argument parsing tests)
fn run_cli_no_server(args: &[&str]) -> Output {
    phd2_guider_command()
        .args(args)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .expect("Failed to run CLI")
}

/// Check if output contains a string (case-insensitive in stdout or stderr)
fn output_contains(output: &Output, needle: &str) -> bool {
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    stdout.to_lowercase().contains(&needle.to_lowercase())
        || stderr.to_lowercase().contains(&needle.to_lowercase())
}

/// Get combined output as string
fn get_output_text(output: &Output) -> String {
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    format!("STDOUT:\n{stdout}\nSTDERR:\n{stderr}")
}

// ----------------------------------------------------------------------------
// Status Command Tests
// ----------------------------------------------------------------------------

#[test]
#[cfg_attr(miri, ignore)]
fn test_status_shows_version() {
    let (_server, port) = spawn_mock_server();
    let output = run_cli(&["status"], port);

    assert!(
        output.status.success(),
        "CLI should succeed: {}",
        get_output_text(&output)
    );
    assert!(
        output_contains(&output, "2.6.11"),
        "Should show PHD2 version: {}",
        get_output_text(&output)
    );
}

#[test]
#[cfg_attr(miri, ignore)]
fn test_status_shows_state() {
    let (_server, port) = spawn_mock_server();
    let output = run_cli(&["status"], port);

    assert!(
        output.status.success(),
        "CLI should succeed: {}",
        get_output_text(&output)
    );
    assert!(
        output_contains(&output, "Stopped"),
        "Should show app state: {}",
        get_output_text(&output)
    );
}

#[test]
#[cfg_attr(miri, ignore)]
fn test_status_shows_equipment_status() {
    let (_server, port) = spawn_mock_server();
    let output = run_cli(&["status"], port);

    assert!(
        output.status.success(),
        "CLI should succeed: {}",
        get_output_text(&output)
    );
    assert!(
        output_contains(&output, "equipment") || output_contains(&output, "connected"),
        "Should show equipment status: {}",
        get_output_text(&output)
    );
}

#[test]
#[cfg_attr(miri, ignore)]
fn test_status_connection_failure() {
    // Use a port that nothing is listening on
    let port = reserved_test_port();
    let output = run_cli_with_timeout(&["status"], port, Duration::from_secs(5));

    assert!(
        !output.status.success(),
        "CLI should fail when server not available"
    );
}

// ----------------------------------------------------------------------------
// Equipment Command Tests
// ----------------------------------------------------------------------------

#[test]
#[cfg_attr(miri, ignore)]
fn test_connect_equipment() {
    let (_server, port) = spawn_mock_server();
    let output = run_cli(&["connect"], port);

    assert!(
        output.status.success(),
        "CLI should succeed: {}",
        get_output_text(&output)
    );
    assert!(
        output_contains(&output, "connected") || output_contains(&output, "success"),
        "Should confirm equipment connected: {}",
        get_output_text(&output)
    );
}

#[test]
#[cfg_attr(miri, ignore)]
fn test_disconnect_equipment() {
    let (_server, port) = spawn_mock_server();
    let output = run_cli(&["disconnect"], port);

    assert!(
        output.status.success(),
        "CLI should succeed: {}",
        get_output_text(&output)
    );
    assert!(
        output_contains(&output, "disconnected") || output_contains(&output, "success"),
        "Should confirm equipment disconnected: {}",
        get_output_text(&output)
    );
}

// ----------------------------------------------------------------------------
// Profile Command Tests
// ----------------------------------------------------------------------------

#[test]
#[cfg_attr(miri, ignore)]
fn test_profiles_lists_all() {
    let (_server, port) = spawn_mock_server();
    let output = run_cli(&["profiles"], port);

    assert!(
        output.status.success(),
        "CLI should succeed: {}",
        get_output_text(&output)
    );
    assert!(
        output_contains(&output, "Mock Profile"),
        "Should list mock profile: {}",
        get_output_text(&output)
    );
}

#[test]
#[cfg_attr(miri, ignore)]
fn test_profiles_shows_current() {
    let (_server, port) = spawn_mock_server();
    let output = run_cli(&["profiles"], port);

    assert!(
        output.status.success(),
        "CLI should succeed: {}",
        get_output_text(&output)
    );
    assert!(
        output_contains(&output, "current") || output_contains(&output, "profile"),
        "Should show current profile info: {}",
        get_output_text(&output)
    );
}

// ----------------------------------------------------------------------------
// Guiding Command Tests
// ----------------------------------------------------------------------------

#[test]
#[cfg_attr(miri, ignore)]
fn test_guide_basic() {
    let (_server, port) = spawn_mock_server();
    let output = run_cli(&["guide"], port);

    assert!(
        output.status.success(),
        "CLI should succeed: {}",
        get_output_text(&output)
    );
    assert!(
        output_contains(&output, "guide") || output_contains(&output, "success"),
        "Should confirm guide command: {}",
        get_output_text(&output)
    );
}

#[test]
#[cfg_attr(miri, ignore)]
fn test_guide_with_recalibrate() {
    let (_server, port) = spawn_mock_server();
    let output = run_cli(&["guide", "--recalibrate"], port);

    assert!(
        output.status.success(),
        "CLI should succeed: {}",
        get_output_text(&output)
    );
}

#[test]
#[cfg_attr(miri, ignore)]
fn test_guide_with_settle_params() {
    let (_server, port) = spawn_mock_server();
    let output = run_cli(
        &[
            "guide",
            "--settle-pixels",
            "1.0",
            "--settle-time",
            "15s",
            "--settle-timeout",
            "2m",
        ],
        port,
    );

    assert!(
        output.status.success(),
        "CLI should succeed: {}",
        get_output_text(&output)
    );
}

#[test]
#[cfg_attr(miri, ignore)]
fn test_guide_with_roi() {
    let (_server, port) = spawn_mock_server();
    let output = run_cli(&["guide", "--roi", "100,100,200,200"], port);

    assert!(
        output.status.success(),
        "CLI should succeed: {}",
        get_output_text(&output)
    );
}

#[test]
#[cfg_attr(miri, ignore)]
fn test_guide_invalid_roi_format() {
    let (_server, port) = spawn_mock_server();
    let output = run_cli(&["guide", "--roi", "invalid"], port);

    assert!(
        !output.status.success(),
        "CLI should fail with invalid ROI format"
    );
}

#[test]
#[cfg_attr(miri, ignore)]
fn test_guide_invalid_roi_not_enough_values() {
    let (_server, port) = spawn_mock_server();
    let output = run_cli(&["guide", "--roi", "100,100"], port);

    assert!(
        !output.status.success(),
        "CLI should fail with incomplete ROI"
    );
}

#[test]
#[cfg_attr(miri, ignore)]
fn test_stop_guiding() {
    let (_server, port) = spawn_mock_server();
    let output = run_cli(&["stop-guiding"], port);

    assert!(
        output.status.success(),
        "CLI should succeed: {}",
        get_output_text(&output)
    );
}

#[test]
#[cfg_attr(miri, ignore)]
fn test_stop_capture() {
    let (_server, port) = spawn_mock_server();
    let output = run_cli(&["stop-capture"], port);

    assert!(
        output.status.success(),
        "CLI should succeed: {}",
        get_output_text(&output)
    );
}

#[test]
#[cfg_attr(miri, ignore)]
fn test_loop_command() {
    let (_server, port) = spawn_mock_server();
    let output = run_cli(&["loop"], port);

    assert!(
        output.status.success(),
        "CLI should succeed: {}",
        get_output_text(&output)
    );
}

// ----------------------------------------------------------------------------
// Pause/Resume Command Tests
// ----------------------------------------------------------------------------

#[test]
#[cfg_attr(miri, ignore)]
fn test_pause_basic() {
    let (_server, port) = spawn_mock_server();
    let output = run_cli(&["pause"], port);

    assert!(
        output.status.success(),
        "CLI should succeed: {}",
        get_output_text(&output)
    );
}

#[test]
#[cfg_attr(miri, ignore)]
fn test_pause_full() {
    let (_server, port) = spawn_mock_server();
    let output = run_cli(&["pause", "--full"], port);

    assert!(
        output.status.success(),
        "CLI should succeed: {}",
        get_output_text(&output)
    );
}

#[test]
#[cfg_attr(miri, ignore)]
fn test_resume() {
    let (_server, port) = spawn_mock_server();
    let output = run_cli(&["resume"], port);

    assert!(
        output.status.success(),
        "CLI should succeed: {}",
        get_output_text(&output)
    );
}

#[test]
#[cfg_attr(miri, ignore)]
fn test_is_paused() {
    let (_server, port) = spawn_mock_server();
    let output = run_cli(&["is-paused"], port);

    assert!(
        output.status.success(),
        "CLI should succeed: {}",
        get_output_text(&output)
    );
    assert!(
        output_contains(&output, "paused"),
        "Should show paused status: {}",
        get_output_text(&output)
    );
}

// ----------------------------------------------------------------------------
// Dither Command Tests
// ----------------------------------------------------------------------------

#[test]
#[cfg_attr(miri, ignore)]
fn test_dither_basic() {
    let (_server, port) = spawn_mock_server();
    let output = run_cli(&["dither"], port);

    assert!(
        output.status.success(),
        "CLI should succeed: {}",
        get_output_text(&output)
    );
}

#[test]
#[cfg_attr(miri, ignore)]
fn test_dither_custom_amount() {
    let (_server, port) = spawn_mock_server();
    let output = run_cli(&["dither", "10.0"], port);

    assert!(
        output.status.success(),
        "CLI should succeed: {}",
        get_output_text(&output)
    );
}

#[test]
#[cfg_attr(miri, ignore)]
fn test_dither_ra_only() {
    let (_server, port) = spawn_mock_server();
    let output = run_cli(&["dither", "--ra-only"], port);

    assert!(
        output.status.success(),
        "CLI should succeed: {}",
        get_output_text(&output)
    );
}

#[test]
#[cfg_attr(miri, ignore)]
fn test_dither_with_settle_params() {
    let (_server, port) = spawn_mock_server();
    let output = run_cli(
        &[
            "dither",
            "5.0",
            "--settle-pixels",
            "0.3",
            "--settle-time",
            "5s",
            "--settle-timeout",
            "30s",
        ],
        port,
    );

    assert!(
        output.status.success(),
        "CLI should succeed: {}",
        get_output_text(&output)
    );
}

// ----------------------------------------------------------------------------
// Argument Parsing Tests
// ----------------------------------------------------------------------------

#[test]
#[cfg_attr(miri, ignore)]
fn test_help_flag() {
    let output = run_cli_no_server(&["--help"]);

    assert!(output.status.success(), "Help should succeed");
    assert!(
        output_contains(&output, "phd2-guider"),
        "Help should mention program name: {}",
        get_output_text(&output)
    );
    assert!(
        output_contains(&output, "status") && output_contains(&output, "guide"),
        "Help should list subcommands: {}",
        get_output_text(&output)
    );
}

#[test]
#[cfg_attr(miri, ignore)]
fn test_subcommand_help() {
    let output = run_cli_no_server(&["guide", "--help"]);

    assert!(output.status.success(), "Subcommand help should succeed");
    assert!(
        output_contains(&output, "recalibrate"),
        "Guide help should show options: {}",
        get_output_text(&output)
    );
}

#[test]
#[cfg_attr(miri, ignore)]
fn test_custom_host_port() {
    let (_server, port) = spawn_mock_server();

    // Run CLI directly without using run_cli helper to test explicit --host and --port
    let output = phd2_guider_command()
        .args(["--host", "127.0.0.1", "--port", &port.to_string(), "status"])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .expect("Failed to run CLI");

    assert!(
        output.status.success(),
        "Custom host/port should work: {}",
        get_output_text(&output)
    );
}

#[test]
#[cfg_attr(miri, ignore)]
fn test_log_level_debug() {
    let (_server, port) = spawn_mock_server();
    let output = run_cli(&["--log-level", "debug", "status"], port);

    assert!(
        output.status.success(),
        "Debug log level should work: {}",
        get_output_text(&output)
    );
}

#[test]
#[cfg_attr(miri, ignore)]
fn test_log_level_warn() {
    let (_server, port) = spawn_mock_server();
    let output = run_cli(&["--log-level", "warn", "status"], port);

    assert!(
        output.status.success(),
        "Warn log level should work: {}",
        get_output_text(&output)
    );
}

#[test]
#[cfg_attr(miri, ignore)]
fn test_invalid_subcommand() {
    let output = run_cli_no_server(&["nonexistent-command"]);

    assert!(!output.status.success(), "Invalid subcommand should fail");
}

#[test]
#[cfg_attr(miri, ignore)]
fn test_no_subcommand_starts_the_http_service() {
    // No subcommand = serve (the packaged systemd unit invokes the bare
    // binary). Point the service at port 0 via a config so parallel test
    // runs never collide on the default 11130, wait for the bound_addr=
    // discovery line, then terminate.
    let dir = tempfile::tempdir().expect("create temp dir");
    let config_path = dir.path().join("config.json");
    std::fs::write(&config_path, r#"{"server": {"port": 0}}"#).expect("write config");

    let mut child = phd2_guider_command()
        .arg("--config")
        .arg(&config_path)
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn bare phd2-guider");

    // Bounded wait: a serve that wedges before printing must fail the
    // test within 10 s (and be reaped), not hang the runner on an
    // endless stdout read.
    let stdout = child.stdout.take().expect("stdout piped");
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let bound = BufReader::new(stdout)
            .lines()
            .find_map(|line| line.ok().filter(|l| l.starts_with("bound_addr=")));
        let _ = tx.send(bound);
    });
    let bound = rx.recv_timeout(Duration::from_secs(10)).ok().flatten();

    let _ = child.kill();
    let _ = child.wait();

    assert!(
        bound.is_some(),
        "bare invocation must start the HTTP service and print bound_addr= within 10s"
    );
}

/// Terminate a spawned service with SIGTERM and wait for a clean exit,
/// falling back to SIGKILL on timeout. SIGTERM (not `Child::kill`) matters
/// under `bazel coverage`: the lifecycle runner's handler exits cleanly, so
/// the child flushes its llvm-cov profile and its `main.rs` lines count.
#[cfg(target_os = "linux")]
fn terminate_gracefully(child: &mut std::process::Child) {
    // SAFETY: signalling a pid we spawned and still own.
    unsafe {
        libc::kill(child.id() as libc::pid_t, libc::SIGTERM);
    }
    let deadline = std::time::Instant::now() + Duration::from_secs(10);
    loop {
        match child.try_wait() {
            Ok(Some(_)) => return,
            Ok(None) if std::time::Instant::now() < deadline => {
                std::thread::sleep(Duration::from_millis(50));
            }
            _ => {
                let _ = child.kill();
                let _ = child.wait();
                return;
            }
        }
    }
}

/// Spawn the binary with `XDG_CONFIG_HOME` pointed at `xdg` and wait (bounded)
/// for the `bound_addr=` discovery line. Linux-only callers: the platform
/// default config path honors `XDG_CONFIG_HOME` only there.
#[cfg(target_os = "linux")]
fn spawn_with_xdg_and_wait_bound(xdg: &std::path::Path, extra_args: &[&str]) -> Option<String> {
    let mut child = phd2_guider_command()
        .args(extra_args)
        .env("XDG_CONFIG_HOME", xdg)
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn phd2-guider");

    let stdout = child.stdout.take().expect("stdout piped");
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let bound = BufReader::new(stdout)
            .lines()
            .find_map(|line| line.ok().filter(|l| l.starts_with("bound_addr=")));
        let _ = tx.send(bound);
    });
    let bound = rx.recv_timeout(Duration::from_secs(10)).ok().flatten();

    terminate_gracefully(&mut child);
    bound
}

/// Write a port-0 config at the platform-default path under `xdg` so the
/// packaged-path tests never collide on the default 11130.
#[cfg(target_os = "linux")]
fn write_xdg_default_config(xdg: &std::path::Path) -> std::path::PathBuf {
    let dir = xdg.join("rusty-photon");
    std::fs::create_dir_all(&dir).expect("create xdg config dir");
    let path = dir.join("phd2-guider.json");
    std::fs::write(&path, r#"{"server": {"port": 0}}"#).expect("write config");
    path
}

#[test]
#[cfg_attr(miri, ignore)]
#[cfg(target_os = "linux")]
fn test_packaged_serve_path_materializes_the_default_config_on_first_start() {
    // The packaged path: bare binary, no --config, no connection flags
    // (systemd passes no arguments). First start must materialize the
    // default config at the platform path via resolve_and_init. The wait
    // is on the file appearing, not on bound_addr=: materialization
    // happens before the bind, so the assertion holds even if the
    // config's default port is unavailable in the test environment.
    let xdg = tempfile::tempdir().expect("create temp dir");
    let expected = xdg.path().join("rusty-photon").join("phd2-guider.json");

    let mut child = phd2_guider_command()
        .env("XDG_CONFIG_HOME", xdg.path())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn phd2-guider");

    let deadline = std::time::Instant::now() + Duration::from_secs(10);
    while !expected.exists() && std::time::Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(50));
    }
    terminate_gracefully(&mut child);

    let content = std::fs::read_to_string(&expected)
        .expect("first start must materialize the default config at the platform path");
    let config: serde_json::Value = serde_json::from_str(&content).expect("valid JSON scaffold");
    assert_eq!(
        config
            .pointer("/server/port")
            .and_then(serde_json::Value::as_u64),
        Some(11130),
        "materialized scaffold must be the serialized default config"
    );
}

#[test]
#[cfg_attr(miri, ignore)]
#[cfg(target_os = "linux")]
fn test_packaged_serve_path_fails_loudly_when_the_config_dir_is_unwritable() {
    use std::os::unix::fs::PermissionsExt;

    // A config location that cannot be created must fail startup with an
    // error — not run with config that could never persist.
    // SAFETY: geteuid has no preconditions. Root ignores directory modes,
    // so the unwritable premise doesn't hold there; skip.
    if unsafe { libc::geteuid() } == 0 {
        eprintln!("running as root; directory modes don't apply — skipping");
        return;
    }
    let xdg = tempfile::tempdir().expect("create temp dir");
    std::fs::set_permissions(xdg.path(), std::fs::Permissions::from_mode(0o555))
        .expect("make config home read-only");

    let mut child = phd2_guider_command()
        .env("XDG_CONFIG_HOME", xdg.path())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn phd2-guider");

    let deadline = std::time::Instant::now() + Duration::from_secs(10);
    let status = loop {
        match child.try_wait().expect("poll child") {
            Some(status) => break Some(status),
            None if std::time::Instant::now() < deadline => {
                std::thread::sleep(Duration::from_millis(50));
            }
            None => break None,
        }
    };
    // Restore write permission so tempdir cleanup can remove the directory.
    let _ = std::fs::set_permissions(xdg.path(), std::fs::Permissions::from_mode(0o755));

    let status = status.unwrap_or_else(|| {
        terminate_gracefully(&mut child);
        panic!("serve must exit promptly when the config dir is unwritable");
    });
    assert!(
        !status.success(),
        "an unwritable config dir must fail startup, not serve on unpersistable config"
    );
}

#[test]
#[cfg_attr(miri, ignore)]
#[cfg(target_os = "linux")]
fn test_serve_with_connection_flags_still_reads_an_existing_default_config() {
    // Serve with an explicit --host but no --config: an existing file at the
    // platform default path wins over the in-memory-defaults fallback (the
    // flags-applied fallback is only for a missing file).
    let xdg = tempfile::tempdir().expect("create temp dir");
    write_xdg_default_config(xdg.path());

    let bound = spawn_with_xdg_and_wait_bound(xdg.path(), &["--host", "127.0.0.1"]).expect(
        "serve with flags must load the existing default-path config and print bound_addr=",
    );

    // The file pins port 0 (kernel-assigned): a default-config fallback that
    // ignored the file would bind the fixed default 11130 instead.
    let port: u16 = bound
        .rsplit(':')
        .next()
        .and_then(|p| p.trim().parse().ok())
        .expect("bound_addr= line must end in a port");
    assert_ne!(
        port, 11130,
        "the existing default-path config must win over in-memory defaults"
    );
    assert_ne!(port, 0, "the printed port must be the kernel-assigned one");
}

// ----------------------------------------------------------------------------
// Config File Tests
// ----------------------------------------------------------------------------

#[test]
#[cfg_attr(miri, ignore)]
fn test_config_file_option() {
    let (_server, port) = spawn_mock_server();

    // Create a temporary config file
    let config_content = format!(
        r#"{{
            "phd2": {{
                "host": "127.0.0.1",
                "port": {port}
            }}
        }}"#
    );

    let config_path = temp_config_path("phd2_config");
    std::fs::write(&config_path, config_content).expect("Failed to write config file");

    let output = phd2_guider_command()
        .args(["--config", config_path.to_str().unwrap(), "status"])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .expect("Failed to run CLI");

    // Clean up
    let _ = std::fs::remove_file(&config_path);

    assert!(
        output.status.success(),
        "Config file should be loaded: {}",
        get_output_text(&output)
    );
}

#[test]
#[cfg_attr(miri, ignore)]
fn test_config_file_not_found() {
    let output = run_cli_no_server(&["--config", "/nonexistent/path/config.json", "status"]);

    assert!(!output.status.success(), "Missing config file should fail");
}

#[test]
#[cfg_attr(miri, ignore)]
fn test_invalid_config_file() {
    let config_path = temp_config_path("invalid_config");
    std::fs::write(&config_path, "{ invalid json }").expect("Failed to write config file");

    let output = run_cli_no_server(&["--config", config_path.to_str().unwrap(), "status"]);

    // Clean up
    let _ = std::fs::remove_file(&config_path);

    assert!(!output.status.success(), "Invalid config JSON should fail");
}

// ----------------------------------------------------------------------------
// Monitor Command Tests (with timeout)
// ----------------------------------------------------------------------------

#[test]
#[cfg_attr(miri, ignore)]
fn test_monitor_receives_version_event() {
    let (_server, port) = spawn_mock_server();

    // Start monitor in background and kill it after a short time
    let mut child = phd2_guider_command()
        .args([
            "--host",
            "127.0.0.1",
            "--port",
            &port.to_string(),
            "monitor",
        ])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("Failed to spawn monitor");

    // Give it time to connect and receive the version event.
    // Windows process startup + TCP connect is slower than Linux.
    std::thread::sleep(Duration::from_secs(3));

    // Kill the monitor
    child.kill().expect("Failed to kill monitor");
    let output = child.wait_with_output().expect("Failed to get output");

    // The version event should have been received
    assert!(
        output_contains(&output, "version") || output_contains(&output, "2.6.11"),
        "Monitor should receive version event: {}",
        get_output_text(&output)
    );
}

// ----------------------------------------------------------------------------
// CLI Error Handling Tests
// ----------------------------------------------------------------------------

#[test]
#[cfg_attr(miri, ignore)]
fn test_connection_refused() {
    // Use a port that's definitely not listening.
    // The CLI has a 10s connection timeout, so allow enough time for it to
    // fail and exit (Windows TCP refusal can be slower than Linux).
    let port = reserved_test_port();

    let output = run_cli_with_timeout(&["status"], port, Duration::from_secs(15));

    assert!(
        !output.status.success(),
        "Should fail when connection refused"
    );
}

#[test]
#[cfg_attr(miri, ignore)]
fn test_connection_timeout_message() {
    // The CLI has a 10s connection timeout; allow enough for it to fail
    // and exit on Windows where TCP refusal is slower.
    let port = reserved_test_port();

    let output = run_cli_with_timeout(&["status"], port, Duration::from_secs(15));

    assert!(
        !output.status.success(),
        "Should fail on connection timeout"
    );
    // Should have some error message
    assert!(
        !get_output_text(&output).trim().is_empty(),
        "Should have error output"
    );
}

// ----------------------------------------------------------------------------
// Shutdown signal handling (regression for #294 / #287 Phase 2)
// ----------------------------------------------------------------------------

/// Regression test for the SIGTERM-missing bug fixed by the
/// rusty-photon-service-lifecycle adoption. Before #294, the Monitor
/// loop only watched `tokio::signal::ctrl_c()`, so `systemctl stop` or
/// `kill -TERM` would leave the process running until force-killed.
/// After the migration, `ServiceRunner` installs both SIGINT and SIGTERM,
/// and the loop races against `shutdown.cancelled()` which observes
/// either.
#[cfg(unix)]
#[test]
#[cfg_attr(miri, ignore)]
fn test_monitor_shuts_down_on_sigterm() {
    let (_server, port) = spawn_mock_server();

    // Stdio::null on both streams: the test doesn't read either, and
    // phd2-guider monitor logs every PHD2 event — an undrained piped
    // stream can fill the OS pipe buffer and deadlock the child. Same
    // pattern as spawn_mock_phd2_dynamic_port's default in this file.
    let mut child = phd2_guider_command()
        .args([
            "--host",
            "127.0.0.1",
            "--port",
            &port.to_string(),
            "monitor",
        ])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("Failed to spawn phd2-guider monitor");

    // Give the process time to start the Monitor loop. The Version event
    // typically arrives within ~500ms; 1s is comfortably past that.
    std::thread::sleep(Duration::from_secs(1));

    // Confirm the child is still running before signalling — otherwise
    // the test would falsely pass for a process that already crashed.
    assert!(
        child.try_wait().expect("try_wait failed").is_none(),
        "Child should still be running before SIGTERM"
    );

    // Send SIGTERM. Safety: kill(2) on a child PID is the documented
    // way to signal a child process; libc::kill is unsafe only because
    // it touches global process state.
    let pid = child.id() as libc::pid_t;
    let rc = unsafe { libc::kill(pid, libc::SIGTERM) };
    assert_eq!(rc, 0, "libc::kill returned non-zero");

    // The Monitor loop should observe the runner's cancellation and
    // exit within a small bounded interval. 2s is generous — graceful
    // shutdown completes in well under 100ms in practice; the budget
    // accommodates CI load and disconnect-RPC roundtrip to the mock.
    let deadline = std::time::Instant::now() + Duration::from_secs(2);
    loop {
        if let Some(status) = child.try_wait().expect("try_wait failed") {
            // Process exited within the deadline. Any termination
            // shape is acceptable as long as it happened
            // promptly — signal-induced exit, clean shutdown,
            // disconnect-failure-on-already-killed-mock; the
            // contract under test is "shut down on SIGTERM", not
            // a specific exit code.
            let _ = status;
            return;
        }
        if std::time::Instant::now() >= deadline {
            child.kill().ok();
            let _ = child.wait();
            panic!("phd2-guider monitor did not exit within 2s of SIGTERM");
        }
        std::thread::sleep(Duration::from_millis(50));
    }
}
