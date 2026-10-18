#![cfg_attr(coverage_nightly, feature(coverage_attribute))]
// bdd-infra is consumed only by other crates' integration/BDD test harnesses.
// It's test infrastructure dressed up as a library — apply the same panic
// allowances as the tests it supports, rather than treating it as production.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::unreachable,
    clippy::panic,
    clippy::string_slice
)]
//! Shared BDD test infrastructure for rusty-photon services.
//!
//! Provides [`ServiceHandle`] for spawning, managing, and stopping service
//! binaries during BDD and integration tests.
//!
//! # Binary discovery
//!
//! BDD tests require a pre-built service binary. Discovery order:
//!
//! 1. The conventional env var `{PACKAGE_UPPER_SNAKE}_BINARY`
//!    (e.g. `RP_BINARY` for `rp`, `PPBA_DRIVER_BINARY` for `ppba-driver`).
//!    Bazel sets this; Cargo tests can too for explicit overrides.
//! 2. `$CARGO_TARGET_DIR/debug/<pkg>` (or `$CARGO_LLVM_COV_TARGET_DIR/debug/<pkg>`
//!    under `cargo llvm-cov`) when either env var is set. If `CARGO_BUILD_TARGET`
//!    is also set, the triple segment is inserted: `.../<triple>/debug/<pkg>`.
//!    When one of these env vars is set we look *only* there — falling through
//!    to the ancestor walk below could silently pick up a stale, non-instrumented
//!    binary and skip coverage data collection.
//! 3. Walking up from the current directory looking for `target/debug/<pkg>`.
//!    `cargo test -p <pkg>` runs tests with the cwd at the package dir, so
//!    the workspace `target/` is typically one level up.
//!
//! If the binary is not found, the spawn call panics with a diagnostic.
//! Services with feature-gated mock hardware (`ppba-driver`, `qhy-focuser`)
//! must be built with `--all-features` — which is what CI does and what the
//! Bazel `*_mock` binaries encode.
//!
//! # `rp-harness` feature
//!
//! Enabling the `rp-harness` cargo feature exposes the [`rp_harness`] module
//! with higher-level helpers for tests that spawn rp alongside `OmniSim` and/or
//! an event plugin: `OmniSimHandle`, `RpConfigBuilder`, `start_rp`,
//! `WebhookReceiver`, and `McpTestClient`. Services whose
//! tests only need `ServiceHandle` should leave the feature off so they don't
//! pull in axum, reqwest, or rmcp transitively.
//!
//! # `tls-auth` feature
//!
//! Enabling the `tls-auth` cargo feature exposes the [`tls_auth`] module:
//! the shared `PkiFixture` (throwaway CA + service certificate + per-run
//! generated credentials), probe helpers, and the [`tls_auth_smoke_steps!`]
//! macro backing every service's TLS + HTTP Basic Auth `auth.feature`.
//!
//! # Usage
//!
//! ```rust,ignore
//! use bdd_infra::ServiceHandle;
//!
//! let handle = ServiceHandle::start(
//!     env!("CARGO_PKG_NAME"),
//!     "path/to/config.json",
//! ).await;
//!
//! // handle.port, handle.base_url are available
//! // ...
//! handle.stop().await;
//! ```
//!
//! # Miri compatibility
//!
//! BDD tests that spawn child processes cannot run under Miri (`pidfd_spawnp`
//! is unsupported). Use the [`bdd_main!`] macro in your `bdd.rs` entry point
//! to automatically skip the test under Miri:
//!
//! ```rust,ignore
//! bdd_infra::bdd_main! {
//!     use cucumber::World as _;
//!     use world::MyWorld;
//!
//!     MyWorld::cucumber()
//!         .fail_on_skipped()
//!         .run_and_exit("tests/features")
//!         .await;
//! }
//! ```

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

/// Entry-point macro for BDD tests that spawn child processes.
///
/// Under Miri the macro expands to an empty `fn main() {}`, because Miri does
/// not support `pidfd_spawnp` and other process-spawning FFI. Under normal
/// compilation it builds a multi-threaded tokio runtime and drives the body on
/// a dedicated thread with a large stack.
///
/// The large stack matters on Windows: `#[tokio::main]` `block_on`s the body on
/// the *main* thread, whose MSVC default stack is ~1 MB, and cucumber drives
/// each scenario's whole step future tree there. The biggest suite (rp) tips
/// that 1 MB over and the test binary aborts with `STATUS_STACK_OVERFLOW`.
/// Driving from a 16 MB thread (and giving tokio workers the same) removes the
/// cliff without changing behavior on any platform.
///
/// If `BDD_PACKAGE_DIR` is set in the environment, the macro chdirs there
/// before running the body. This lets Bazel run BDD tests where the cwd is
/// the runfiles tree rather than the package directory so that relative
/// paths like `"tests/features"` and `"./Cargo.toml"` behave the same way
/// they do under `cargo test`. Any `*_BINARY` env vars that hold relative
/// paths are rewritten to absolute paths before chdir so binary discovery
/// still resolves against the runfiles root.
///
/// The macro also advertises Bazel test-sharding support (touching
/// `TEST_SHARD_STATUS_FILE` when set — required for targets with
/// `shard_count`). Note that advertising is not partitioning: a sharded
/// suite must additionally route its scenario filter through
/// [`sharding::scenario_in_current_shard`], or every shard runs the whole
/// suite.
///
/// Finally it installs the suite's `tracing` subscriber
/// ([`init_test_tracing`]), so in-process library code a step drives can
/// explain itself when the step fails.
#[macro_export]
macro_rules! bdd_main {
    ($($body:tt)*) => {
        #[cfg(miri)]
        fn main() {}

        #[cfg(not(miri))]
        fn main() {
            $crate::sharding::advertise_bazel_sharding_support();
            $crate::__bdd_bazel_chdir();
            $crate::init_test_tracing();
            // 16 MB driver + worker stacks: see the macro docs — the rp suite
            // overflows Windows' ~1 MB main-thread stack otherwise.
            const BDD_STACK_SIZE: usize = 16 * 1024 * 1024;
            let driver = ::std::thread::Builder::new()
                .name("bdd-main".to_string())
                .stack_size(BDD_STACK_SIZE)
                .spawn(|| {
                    let runtime = ::tokio::runtime::Builder::new_multi_thread()
                        .enable_all()
                        .thread_stack_size(BDD_STACK_SIZE)
                        .build()
                        .expect("bdd_main: build tokio runtime");
                    runtime.block_on(async {
                        $($body)*
                    });
                })
                .expect("bdd_main: spawn driver thread");
            driver.join().expect("bdd_main: driver thread panicked");
        }
    };
}

/// What a BDD suite logs when `RUST_LOG` says nothing.
///
/// Every service under test is a *child process* whose own subscriber and
/// stderr the suite already forwards, so the only events this subscriber can
/// ever see are from library code running **in** the test process. Of that,
/// `rusty-photon-tls`'s server loop is the piece whose failure modes are
/// otherwise unobservable: it answers a connection it cannot serve — a
/// handshake that errored, one that never sent a byte inside the idle bound,
/// a certificate pair that failed to reload — by dropping the connection and
/// saying so at `debug!`, leaving the client with a bare transport error and
/// no reason for it. Everything else stays at `warn` so a passing run's log
/// is unchanged.
const DEFAULT_BDD_LOG: &str = "warn,rusty_photon_tls=debug";

/// Install the suite's `tracing` subscriber — called by [`bdd_main!`] before
/// the suite body runs, so this is the subscriber a `bdd_main!` suite gets.
///
/// The `try_init` below makes a second call anywhere in the process a no-op
/// rather than a panic, which also means a suite cannot swap in its own
/// afterwards: change what it logs through `RUST_LOG`.
///
/// `RUST_LOG` overrides [`DEFAULT_BDD_LOG`] as usual, which is how you turn a
/// suite up while reproducing a failure locally; an unset *or unparseable*
/// one falls back to the default rather than taking the suite down over an
/// environment variable (the convention `rusty-photon-service-lifecycle`'s
/// `build_env_filter` set). ANSI is off: this writes into a Bazel test log,
/// not a terminal.
pub fn init_test_tracing() {
    let filter = tracing_subscriber::EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new(DEFAULT_BDD_LOG));
    let _ = tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_writer(std::io::stderr)
        .with_ansi(false)
        .try_init();
}

#[doc(hidden)]
pub fn __bdd_bazel_chdir() {
    let Ok(dir) = std::env::var("BDD_PACKAGE_DIR") else {
        return;
    };
    let cwd = std::env::current_dir().expect("bdd_main: current_dir");
    // Absolutize relative paths that must survive the chdir below: the
    // `*_BINARY` discovery vars, and (under `bazel coverage`) `COVERAGE_DIR`,
    // from which spawned children derive their `LLVM_PROFILE_FILE` at spawn time
    // — by then the cwd is `BDD_PACKAGE_DIR`, so a relative `COVERAGE_DIR` would
    // resolve wrong. Bazel sets it absolute today; this keeps us correct if it
    // ever doesn't. See [`child_coverage_profile_var`].
    let to_absolutize: Vec<(String, String)> = std::env::vars()
        .filter(|(k, v)| {
            (k.ends_with("_BINARY") || k == "COVERAGE_DIR") && std::path::Path::new(v).is_relative()
        })
        .collect();
    for (k, v) in to_absolutize {
        std::env::set_var(&k, cwd.join(v));
    }
    std::env::set_current_dir(&dir).unwrap_or_else(|e| panic!("bdd_main: chdir to {dir}: {e}"));
}

pub mod doctor_smoke;
pub mod scratch;
pub mod sharding;

#[cfg(feature = "rp-harness")]
pub mod rp_harness;

#[cfg(feature = "rp-harness")]
pub mod sky_survey_camera_harness;

#[cfg(feature = "conformu")]
pub mod conformu;

#[cfg(feature = "conformu")]
pub use conformu::{run_conformu, run_conformu_from_settings, ConformuRun, FullRunSettings};

#[cfg(feature = "tls-auth")]
pub mod tls_auth;

#[cfg(feature = "setup-pages")]
pub mod setup_pages;

/// The band [`reserved_test_port`] draws from: below every platform's
/// ephemeral floor (32768 on Linux, 49152 on Windows and macOS) so the
/// OS can never assign one of these to a `bind(0)` caller, and clear of
/// the services' own fixed ports (11112-11173).
const RESERVED_PORT_BAND_START: u16 = 20_000;
const RESERVED_PORT_BAND_LEN: u16 = 12_000;

/// How many band ports one call will try before giving up. Only a port
/// some other process is on gets skipped, so the ceiling is never
/// approached.
const RESERVED_PORT_TRIES: u16 = 64;

/// A port a test may hand to a child to bind, chosen when the test has
/// to know it *before* the child starts (docs/skills/testing.md §5.1).
///
/// Drawn from a reserved band below the ephemeral floor, entered at a
/// per-process start so concurrent test binaries do not march in step,
/// and probed by `connect()`, never by binding: a listener opened for a
/// probe is duplicated into any child a sibling thread forks in that
/// instant and outlives the close until that child execs, so a bind
/// probe can hand back a port that still answers. A refused connect
/// carries the same "nobody is there" answer and leaves nothing behind.
/// The same shape as `reserved_test_port` in phd2-guider's integration
/// tests, shared here for the suites built on this crate.
///
/// The probe is a synchronous `connect()`, and a refused connect is not
/// instant everywhere: on the Windows CI runner each one cost about 2 s.
/// So it runs on the blocking pool ([`off_loop`]), and the step that
/// needs the port yields instead of stalling every scenario in flight.
/// There is no synchronous form outside this crate.
///
/// # Panics
///
/// Panics if no band port answers "refused" within
/// [`RESERVED_PORT_TRIES`] tries.
pub async fn reserved_test_port_async() -> u16 {
    let port = off_loop(reserved_test_port).await;
    debug!(port, "reserved a test port");
    port
}

/// The port in `slot`, reserving one with [`reserved_test_port_async`] the
/// first time. For a scenario that must keep one port across rewrites of
/// its config, so a reload or a restart rebinds the same port.
///
/// # Panics
///
/// [`reserved_test_port_async`]'s panic.
pub async fn reserved_port_once(slot: &mut Option<u16>) -> u16 {
    if let Some(port) = *slot {
        debug!(port, "reusing the scenario's reserved port");
        return port;
    }
    let port = reserved_test_port_async().await;
    *slot = Some(port);
    port
}

/// The probe behind [`reserved_test_port_async`], run on the blocking
/// pool. It blocks, so nothing outside this crate may call it.
#[must_use]
pub(crate) fn reserved_test_port() -> u16 {
    static START: std::sync::OnceLock<u16> = std::sync::OnceLock::new();
    static CURSOR: std::sync::atomic::AtomicU16 = std::sync::atomic::AtomicU16::new(0);

    // Distinct per process: pids alone are too regular when a harness
    // starts many copies at once.
    let start = *START.get_or_init(|| {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |d| d.subsec_nanos());
        u16::try_from(
            (std::process::id() ^ nanos)
                .checked_rem(u32::from(RESERVED_PORT_BAND_LEN))
                .unwrap_or(0),
        )
        .unwrap_or(0)
    });

    for _ in 0..RESERVED_PORT_TRIES {
        // A distinct step per call, so no two callers get the same port.
        let step = CURSOR.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let candidate = RESERVED_PORT_BAND_START.saturating_add(
            start
                .wrapping_add(step)
                .wrapping_rem(RESERVED_PORT_BAND_LEN),
        );
        if std::net::TcpStream::connect(("127.0.0.1", candidate)).is_err() {
            return candidate;
        }
    }
    panic!("no free port in {RESERVED_PORT_TRIES} tries");
}

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
mod reserved_port_tests {
    use super::*;

    #[test]
    fn reserved_ports_come_from_the_band_and_differ_per_call() {
        let a = reserved_test_port();
        let b = reserved_test_port();
        for port in [a, b] {
            assert!(
                (RESERVED_PORT_BAND_START..RESERVED_PORT_BAND_START + RESERVED_PORT_BAND_LEN)
                    .contains(&port),
                "{port} is outside the reserved band"
            );
        }
        assert_ne!(a, b);
    }

    /// The poll loop keeps running while `f` blocks. On a single-threaded
    /// runtime a ticker spawned beside the call must keep ticking through
    /// the 300 ms `f` sleeps; run inline, `f` would hold the only thread
    /// and the ticker could not tick once.
    #[tokio::test(flavor = "current_thread")]
    async fn off_loop_leaves_the_poll_loop_running_while_its_closure_blocks() {
        let ticks = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let ticker = {
            let ticks = std::sync::Arc::clone(&ticks);
            tokio::spawn(async move {
                let mut interval = tokio::time::interval(Duration::from_millis(10));
                loop {
                    interval.tick().await;
                    ticks.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                }
            })
        };
        let before = ticks.load(std::sync::atomic::Ordering::Relaxed);
        off_loop(|| std::thread::sleep(Duration::from_millis(300))).await;
        let during = ticks
            .load(std::sync::atomic::Ordering::Relaxed)
            .saturating_sub(before);
        ticker.abort();
        assert!(
            during >= 5,
            "the ticker advanced {during} times while a 300 ms closure blocked"
        );
    }

    #[tokio::test]
    async fn off_loop_fails_its_caller_with_the_closures_panic() {
        let joined = tokio::spawn(off_loop::<()>(|| panic!("the probe exploded"))).await;
        let payload = joined
            .expect_err("a panicking closure must fail its caller")
            .into_panic();
        assert_eq!(
            payload.downcast_ref::<&str>().copied(),
            Some("the probe exploded")
        );
    }

    /// A formatted re-raise, as `shared_pki` makes, reaches the caller as
    /// the very `String` it was given.
    #[tokio::test]
    async fn off_loop_carries_a_string_payload_unchanged() {
        let joined = tokio::spawn(off_loop::<()>(|| {
            std::panic::resume_unwind(Box::new(String::from("PKI generation for x panicked")))
        }))
        .await;
        let payload = joined
            .expect_err("a re-raised panic must fail its caller")
            .into_panic();
        assert_eq!(
            payload.downcast_ref::<String>().map(String::as_str),
            Some("PKI generation for x panicked")
        );
    }

    #[test]
    fn a_spawn_that_panicked_under_the_lock_does_not_fail_the_next() {
        let panicked = std::panic::catch_unwind(|| with_spawn_lock(|| panic!("the spawn blew up")));
        panicked.expect_err("a panic under the spawn lock must reach the caller");
        assert_eq!(with_spawn_lock(|| 7), 7);
    }

    #[test]
    fn ephemeral_ports_are_distinct_and_bindable() {
        let ports = ephemeral_ports::<3>();
        assert!(ports.iter().all(|&port| port != 0), "{ports:?}");
        assert!(
            ports[0] != ports[1] && ports[1] != ports[2] && ports[0] != ports[2],
            "{ports:?}"
        );
        for port in ports {
            std::net::TcpListener::bind(("127.0.0.1", port))
                .unwrap_or_else(|e| panic!("port {port} was not released: {e}"));
        }
    }

    /// The async form waits for the lock without blocking the loop: on a
    /// single-threaded runtime, a closure under the synchronous form on a
    /// pool thread holds the lock while the loop keeps ticking, and the
    /// async form gets it once that closure is done.
    #[tokio::test(flavor = "current_thread")]
    async fn the_async_form_waits_for_the_lock_without_blocking_the_loop() {
        let (held_tx, held_rx) = tokio::sync::oneshot::channel();
        let holder = off_loop(move || {
            with_spawn_lock(|| {
                held_tx.send(()).expect("the test is waiting");
                std::thread::sleep(Duration::from_millis(200));
            });
        });
        let holder = tokio::spawn(holder);
        held_rx.await.expect("the holder took the lock");
        let ticks = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let ticker = {
            let ticks = std::sync::Arc::clone(&ticks);
            tokio::spawn(async move {
                let mut interval = tokio::time::interval(Duration::from_millis(10));
                loop {
                    interval.tick().await;
                    ticks.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                }
            })
        };
        assert_eq!(with_spawn_lock_async(|| 7).await, 7);
        let during = ticks.load(std::sync::atomic::Ordering::Relaxed);
        ticker.abort();
        holder.await.expect("the holder finished");
        assert!(
            during >= 5,
            "the loop ticked {during} times while the async form waited for the lock"
        );
    }

    #[test]
    fn panic_text_reads_both_string_payloads() {
        let literal: Box<dyn std::any::Any + Send> = Box::new("a literal");
        let formatted: Box<dyn std::any::Any + Send> = Box::new(String::from("a formatted one"));
        let other: Box<dyn std::any::Any + Send> = Box::new(7_u8);
        assert_eq!(panic_text(literal.as_ref()), "a literal");
        assert_eq!(panic_text(formatted.as_ref()), "a formatted one");
        assert_eq!(panic_text(other.as_ref()), "a panic without a message");
    }

    #[tokio::test]
    async fn a_reserved_port_is_kept_once_reserved() {
        let mut slot = None;
        let first = reserved_port_once(&mut slot).await;
        assert_eq!(slot, Some(first));
        assert_eq!(reserved_port_once(&mut slot).await, first);
    }

    #[tokio::test]
    async fn the_async_form_draws_from_the_same_band() {
        let port = reserved_test_port_async().await;
        assert!(
            (RESERVED_PORT_BAND_START..RESERVED_PORT_BAND_START + RESERVED_PORT_BAND_LEN)
                .contains(&port),
            "{port} is outside the reserved band"
        );
    }
}

use std::process::Stdio;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use tokio::io::{AsyncBufReadExt, BufReader};
use tracing::debug;

/// Derive the conventional env var name for a package's binary override.
///
/// `ppba-driver` → `PPBA_DRIVER_BINARY`, `rp` → `RP_BINARY`, and so on.
fn binary_env_var(package_name: &str) -> String {
    format!("{}_BINARY", package_name.to_uppercase().replace('-', "_"))
}

/// Monotonic counter handing out a unique `<package>#<seq>` label to every
/// spawned [`ServiceHandle`] in this test process.
///
/// Cucumber runs up to 64 scenarios concurrently by default, and most BDD
/// suites don't tag their features `@serial`, so many same-named service
/// instances (e.g. several scenarios each discovering a stub as
/// "plate-solver") can be alive at once, each a separate child process. Every
/// child's `tracing` output goes to stderr, which — absent this label —
/// merges into one shared, unattributed stream with no way to tell which
/// lines belong to which instance (see issue #578, where this ambiguity made
/// a CI flake look like a single continuously-unhealthy service when it was
/// most likely several concurrent instances' logs interleaved).
static SPAWN_SEQ: AtomicU64 = AtomicU64::new(0);

fn next_spawn_label(package_name: &str) -> String {
    format!(
        "{package_name}#{}",
        SPAWN_SEQ.fetch_add(1, Ordering::Relaxed)
    )
}

/// Per-child `LLVM_PROFILE_FILE` for coverage collection under `bazel coverage`.
///
/// Under `bazel coverage` `rules_rust` instruments the first-party service
/// binaries and sets `COVERAGE_DIR` for the test action, but only the test
/// process's own `.profraw` is collected by default — a spawned child inherits
/// the parent test's `LLVM_PROFILE_FILE` and would clobber it. Point each child
/// at its own online-merge pool inside `COVERAGE_DIR` (`<pkg>-%8m.profraw`) so
/// Bazel's lcov merger — which globs that directory — folds the child's coverage
/// into the combined report, while the `%Nm` pool keeps the raw-profile count
/// (and the bytes Bazel must stage after the test) bounded. This is the
/// child-process-coverage contingency from `docs/plans/archive/bazel-migration.md`.
///
/// Returns `None` when `COVERAGE_DIR` is unset: under plain `bazel test`, and
/// under `cargo`/`cargo-llvm-cov` (which sets `LLVM_PROFILE_FILE`, not
/// `COVERAGE_DIR`), leaving those paths untouched.
fn child_coverage_profile_var(package_name: &str) -> Option<(&'static str, std::ffi::OsString)> {
    let coverage_dir = std::env::var_os("COVERAGE_DIR")?;
    Some((
        "LLVM_PROFILE_FILE",
        child_coverage_profile_path(&coverage_dir, package_name),
    ))
}

/// Build the `<COVERAGE_DIR>/<pkg>-%8m.profraw` path. Split out from
/// [`child_coverage_profile_var`] so the path construction is unit-testable
/// without mutating process env.
///
/// `%8m` (binary signature with an 8-file online-merge POOL), NOT `%p-%m`. The
/// `%p` (pid) made a SEPARATE file per child PROCESS: across `rp:bdd`'s ~265
/// scenarios each spawning `rp` + `sky-survey-camera`, that is hundreds of
/// ~6 MB `.profraw` (~1.5 GB) that Bazel's sandbox must stage/tear down after
/// the test — the dominant cost of the post-`[Summary]` coverage phase. `%Nm`
/// instead has the LLVM runtime merge each process's counters on exit into a
/// bounded pool of N files per binary signature (file-locked), so the same ~265
/// processes collapse to <=8 files (~50 MB). Verified empirically: 20 runs of an
/// instrumented binary => 20 files with `%p-%m`, 8 files with `%8m`. The
/// `<pkg>-` prefix keeps each service's pool distinct from the others and from
/// the test binary's own profraw.
fn child_coverage_profile_path(
    coverage_dir: &std::ffi::OsStr,
    package_name: &str,
) -> std::ffi::OsString {
    let mut path = std::path::PathBuf::from(coverage_dir);
    path.push(format!("{package_name}-%8m.profraw"));
    path.into_os_string()
}

/// Handle to a running service process.
///
/// Manages the full lifecycle: binary discovery, spawning with stdout
/// capture, port parsing, graceful shutdown signaling, stdout draining, and
/// labeled stderr forwarding.
///
/// On [`Drop`], sends a best-effort graceful-shutdown signal (SIGTERM on Unix,
/// `CTRL_BREAK_EVENT` on Windows) before the child handle is dropped. Callers
/// should use [`stop`](ServiceHandle::stop) when they need an explicit
/// graceful shutdown path, because dropping the handle may still force the
/// process to terminate if it has not already exited.
///
/// Its async methods run inside cucumber steps, which all share one poll
/// loop (see [`run_once_async`]), so nothing they do may block it: the
/// spawn itself runs on the blocking pool, and every wait is awaited. Keep
/// it that way in anything added here.
#[derive(Debug)]
pub struct ServiceHandle {
    child: Option<tokio::process::Child>,
    /// The port the service bound to (parsed from stdout).
    pub port: u16,
    /// The base URL of the running service (e.g., `http://127.0.0.1:12345`).
    pub base_url: String,
    stdout_drain: Option<tokio::task::JoinHandle<()>>,
    stderr_forward: Option<tokio::task::JoinHandle<()>>,
    /// Service name (for log/error messages).
    name: String,
}

impl ServiceHandle {
    /// Start a service binary with the given config file.
    ///
    /// # Arguments
    ///
    /// * `package_name` — pass `env!("CARGO_PKG_NAME")` from the calling crate
    /// * `config_path` — path to the service config file (typically a temp file)
    ///
    /// # Binary discovery
    ///
    /// See the module-level docs. Panics with a clear diagnostic if the binary
    /// is not found — BDD binaries must be pre-built (e.g.
    /// `cargo build --all-features --all-targets`).
    pub async fn start(package_name: &str, config_path: &str) -> Self {
        Self::start_with_args(package_name, &["--config", config_path]).await
    }

    /// Start a service binary with an explicit argument vector.
    ///
    /// [`start`](Self::start) is the common case (`--config <path>`). Use this
    /// when a scenario needs extra flags — e.g. a CLI override the service
    /// reports as pinned in its config (`--port`, `--server-port`). The args
    /// are passed through verbatim, so the caller supplies `--config` itself.
    ///
    /// Same binary discovery and panic-on-missing behaviour as [`start`](Self::start).
    pub async fn start_with_args(package_name: &str, args: &[&str]) -> Self {
        Self::start_with_env(package_name, args, &[]).await
    }

    /// Like [`start_with_args`](Self::start_with_args), additionally setting
    /// environment variables on the child process.
    ///
    /// Scenarios run concurrently inside one test process, so a child that
    /// needs a per-scenario environment (e.g. sentinel's
    /// `SENTINEL_SERVICE_MANAGER_DIR` stub seam) must receive it here rather
    /// than via `std::env::set_var`, which would race across scenarios.
    ///
    /// # Panics
    ///
    /// Panics if the binary cannot be found or spawned, if its stdout or
    /// stderr pipe cannot be captured, or if it exits without printing a
    /// `bound_addr=` line. There is no deadline: a child that neither prints
    /// one nor exits blocks here —
    /// [`try_start_with_env`](Self::try_start_with_env) bounds the wait.
    pub async fn start_with_env(package_name: &str, args: &[&str], envs: &[(&str, &str)]) -> Self {
        let label = next_spawn_label(package_name);

        let mut child = spawn_service(package_name, args, envs).await;

        let stderr = child
            .stderr
            .take()
            .unwrap_or_else(|| panic!("failed to capture {package_name} stderr"));
        let stderr_forward = spawn_stderr_forwarder(stderr, label);

        let stdout = child
            .stdout
            .take()
            .unwrap_or_else(|| panic!("failed to capture {package_name} stdout"));
        let (port, stdout_drain) = parse_bound_port(stdout)
            .await
            .unwrap_or_else(|| panic!("failed to parse bound port from {package_name} output"));

        Self {
            child: Some(child),
            port,
            base_url: format!("http://127.0.0.1:{port}"),
            stdout_drain: Some(stdout_drain),
            stderr_forward: Some(stderr_forward),
            name: package_name.to_string(),
        }
    }

    /// Try to start the service, returning an error instead of panicking when
    /// it fails to come up.
    ///
    /// # Errors
    ///
    /// [`try_start_with_args`](Self::try_start_with_args)'s: a pipe that
    /// cannot be captured, a child that exits without binding, or 30 seconds
    /// without a bound address.
    ///
    /// # Panics
    ///
    /// Still panics if the binary cannot be located or spawned — that's a
    /// setup error, not a runtime condition to recover from.
    pub async fn try_start(package_name: &str, config_path: &str) -> Result<Self, String> {
        Self::try_start_with_args(package_name, &["--config", config_path]).await
    }

    /// Like [`try_start`](Self::try_start) but with an explicit argument vector
    /// (see [`start_with_args`](Self::start_with_args)).
    ///
    /// # Errors
    ///
    /// [`try_start_with_env`](Self::try_start_with_env)'s.
    ///
    /// # Panics
    ///
    /// Panics if the binary cannot be located or spawned, as
    /// [`try_start`](Self::try_start) does.
    pub async fn try_start_with_args(package_name: &str, args: &[&str]) -> Result<Self, String> {
        Self::try_start_with_env(package_name, args, &[]).await
    }

    /// Like [`try_start_with_args`](Self::try_start_with_args), additionally
    /// setting environment variables on the child process (see
    /// [`start_with_env`](Self::start_with_env)).
    ///
    /// # Errors
    ///
    /// Returns a message if the child's stdout or stderr pipe cannot be
    /// captured, if the child exits without printing its bound address (the
    /// exit status is included), or if it has not printed one within 30
    /// seconds (the child is killed).
    ///
    /// # Panics
    ///
    /// Panics if the binary cannot be located or spawned, as
    /// [`try_start`](Self::try_start) does.
    pub async fn try_start_with_env(
        package_name: &str,
        args: &[&str],
        envs: &[(&str, &str)],
    ) -> Result<Self, String> {
        let label = next_spawn_label(package_name);

        let mut child = spawn_service(package_name, args, envs).await;

        let stderr = child
            .stderr
            .take()
            .ok_or_else(|| format!("failed to capture {package_name} stderr"))?;
        let stderr_forward = spawn_stderr_forwarder(stderr, label);

        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| format!("failed to capture {package_name} stdout"))?;

        match tokio::time::timeout(Duration::from_secs(30), parse_bound_port(stdout)).await {
            Ok(Some((port, stdout_drain))) => Ok(Self {
                child: Some(child),
                port,
                base_url: format!("http://127.0.0.1:{port}"),
                stdout_drain: Some(stdout_drain),
                stderr_forward: Some(stderr_forward),
                name: package_name.to_string(),
            }),
            Ok(None) => {
                let status = child.wait().await;
                Err(format!("{package_name} exited without binding: {status:?}"))
            }
            Err(_) => {
                let _ = child.kill().await;
                Err(format!("timeout waiting for {package_name} to bind"))
            }
        }
    }

    /// Returns `true` if the service process is currently running.
    #[must_use]
    pub const fn is_running(&self) -> bool {
        self.child.is_some()
    }

    /// Stop the service gracefully via the platform's shutdown signal
    /// (SIGTERM on Unix, `CTRL_BREAK_EVENT` on Windows), falling back to a
    /// forced kill after 5 seconds.
    ///
    /// Graceful shutdown lets the service run its own stop path — flushing
    /// coverage data (profraw files), draining in-flight work, and persisting
    /// whatever its shutdown handlers persist. The signal only achieves that
    /// if the service actually *handles* it: an unhandled `CTRL_BREAK_EVENT`
    /// is fatal on Windows, and fatal quickly, so it looks like a clean stop
    /// from out here. `test_stop_runs_the_service_shutdown_path` holds that
    /// contract to an observable effect rather than to timing.
    pub async fn stop(&mut self) {
        if let Some(mut child) = self.child.take() {
            if let Some(pid) = child.id() {
                send_sigterm(pid);

                if tokio::time::timeout(Duration::from_secs(5), child.wait())
                    .await
                    .is_err()
                {
                    debug!(
                        "{} did not exit after {GRACEFUL_EVENT}, forcing it down with \
                         {FORCED_STOP}",
                        self.name
                    );
                    let _ = child.kill().await;
                    let _ = child.wait().await;
                }
            } else {
                let _ = child.kill().await;
                let _ = child.wait().await;
            }
        }

        // Stop draining stdout/stderr only *after* the child has exited. With
        // both pipes' write ends now closed, the tasks observe EOF and finish
        // on their own, so we join them rather than aborting them.
        //
        // Aborting a drain *before* the child exits (the previous behaviour,
        // for stdout) closed the read end of that pipe while the child was
        // still running its SIGTERM shutdown path. Every shutdown-path log
        // line the child then wrote hit a broken pipe (EPIPE); because the
        // services build their subscriber with `tracing_subscriber::fmt()`
        // (whose builder defaults `log_internal_errors = true`),
        // tracing-subscriber echoed each failure as
        // "[tracing-subscriber] Unable to write an event ... Broken pipe
        // (os error 32)" to stderr, polluting the BDD/CI logs.
        if let Some(handle) = self.stdout_drain.take() {
            join_or_abort(handle).await;
        }
        if let Some(handle) = self.stderr_forward.take() {
            join_or_abort(handle).await;
        }
    }

    /// Kill the service immediately and forcibly (SIGKILL on Unix,
    /// `TerminateProcess` on Windows) — no shutdown path runs.
    ///
    /// This simulates a crash / power failure for resume and recovery
    /// scenarios; use [`stop`](Self::stop) everywhere else (a forced kill
    /// also forfeits the process's coverage flush). The wait +
    /// stdout-drain handling is delegated to `stop`: the forced kill is
    /// already pending and cannot be handled, so the process never runs
    /// its graceful shutdown path — `stop` just reaps it and joins the
    /// drain.
    pub async fn kill(&mut self) {
        if let Some(child) = self.child.as_mut() {
            let _ = child.start_kill();
        }
        self.stop().await;
    }
}

impl Drop for ServiceHandle {
    fn drop(&mut self) {
        // Request graceful shutdown, but deliberately do NOT abort the stdout
        // drain / stderr forwarder tasks. Aborting closes the read end of the
        // child's pipe while the child is still alive, so the child's
        // shutdown-path log writes hit a broken pipe and tracing-subscriber
        // prints "[tracing-subscriber] Unable to write an event ... Broken
        // pipe" to stderr, polluting test logs (see [`ServiceHandle::stop`]
        // for the full chain). Dropping the `stdout_drain`/`stderr_forward`
        // JoinHandles along with the rest of the struct *detaches* those tasks
        // rather than cancelling them, so their read ends stay open until the
        // child exits; `kill_on_drop` on the child handle guarantees that —
        // and thus their EOF — arrives.
        if let Some(ref mut child) = self.child {
            if let Some(pid) = child.id() {
                send_sigterm(pid);
            } else {
                let _ = child.start_kill();
            }
        }
    }
}

/// Run a service binary once with the given arguments and wait for it to exit.
///
/// Uses the same binary discovery as [`ServiceHandle::start`]. Panics if the
/// binary cannot be found.
///
/// Use this for one-shot commands like `doctor tls issue` that are not
/// long-running servers. When `stdin_data` is `Some`, the data is piped to the
/// process's stdin.
///
/// **A cucumber step must call [`run_once_async`] instead** — see its docs for
/// what a blocking wait costs the suite around it. This synchronous form is
/// for callers with no runtime to yield to: plain `#[test]` integration tests.
pub fn run_once(
    package_name: &str,
    args: &[&str],
    stdin_data: Option<&[u8]>,
) -> std::process::Output {
    let binary = require_binary(package_name);
    debug!(binary = %binary, "running {} from pre-built binary", package_name);
    run_to_completion(package_name, &binary, args, stdin_data)
}

/// [`run_once`], awaited instead of blocked on.
///
/// Cucumber drives every scenario as a `LocalBoxFuture` in **one**
/// `FuturesUnordered` on the [`bdd_main!`] runtime's `block_on` task, so up to
/// 64 scenarios' worth of async work shares a single poll loop. A synchronous
/// wait inside a step stops that loop: for as long as the child runs, no other
/// scenario is polled — in-process test servers stop answering, timers fire
/// late, and steps elsewhere in the suite fail with transport errors and
/// timeouts that read like product bugs. The cost scales with the number of
/// scenarios in flight and with how slowly the host spawns processes, which is
/// why it bites hardest on the 3-4 vCPU CI runners.
///
/// Waiting on the blocking pool instead lets the step yield, so the poll loop
/// keeps running and the suite keeps its concurrency.
///
/// # Panics
///
/// Panics if the binary cannot be found, spawned, fed its stdin, or waited
/// on, with [`run_once`]'s message.
pub async fn run_once_async(
    package_name: &str,
    args: &[&str],
    stdin_data: Option<&[u8]>,
) -> std::process::Output {
    let package = package_name.to_string();
    let args: Vec<String> = args.iter().map(|arg| (*arg).to_string()).collect();
    let stdin_data = stdin_data.map(<[u8]>::to_vec);
    off_loop(move || {
        let args: Vec<&str> = args.iter().map(String::as_str).collect();
        run_once(&package, &args, stdin_data.as_deref())
    })
    .await
}

/// The message a panic carried, for re-raising it with more context: the
/// text `panic!` was given, or a stand-in when the payload is not a string.
#[cfg(any(feature = "tls-auth", test))]
pub(crate) fn panic_text(payload: &(dyn std::any::Any + Send)) -> &str {
    payload
        .downcast_ref::<&str>()
        .copied()
        .or_else(|| payload.downcast_ref::<String>().map(String::as_str))
        .unwrap_or("a panic without a message")
}

/// Run `f` on the blocking pool and await its result, so a step that has
/// to block (wait on a child, probe a port, stat files) yields to the
/// poll loop every scenario shares (see [`run_once_async`]).
///
/// The blocking pool's threads enter the runtime, so `f` may create
/// runtime-bound things there, such as a `tokio::process::Child` whose
/// pipes the caller then awaits.
///
/// Process state tied to the spawning *thread* follows that pool thread,
/// which the pool retires once it has sat idle, not the caller. Linux's
/// `PR_SET_PDEATHSIG` is one: it fires when the thread that forked the
/// child exits, so a child spawned with it through `off_loop` would be
/// killed when the pool thread goes, mid-scenario. Spawn such a child
/// where it is meant to live; the `OmniSim` harness is one.
///
/// # Panics
///
/// Re-raises a panic in `f` with its own payload, so the step fails with
/// `f`'s message. Panics if the runtime shut down before `f` ran.
pub async fn off_loop<T: Send + 'static>(f: impl FnOnce() -> T + Send + 'static) -> T {
    match tokio::task::spawn_blocking(f).await {
        Ok(value) => value,
        Err(e) => match e.try_into_panic() {
            Ok(payload) => std::panic::resume_unwind(payload),
            Err(e) => panic!("the blocking task did not run: {e}"),
        },
    }
}

/// The lock that orders process creation in this test process against
/// other process creation, and against the probes that bind a port only to
/// hand it to a child ([`ephemeral_ports`]).
///
/// [`off_loop`] runs spawns in parallel on blocking-pool threads, where the
/// single poll loop used to run them one at a time. On targets without
/// `pipe2` and `SOCK_CLOEXEC` (macOS among them) std makes a descriptor in
/// two steps, creating it and only then marking it close-on-exec, for a
/// child's stdio pipes and for every socket alike. A fork that lands
/// between the two steps inherits the descriptor: a stray copy of a pipe's
/// write end keeps a child's stdout from ever reaching EOF, and a stray
/// copy of a probe's listener keeps its port bound after the probe let it
/// go. Spawns and those probes all take this lock, so none of them forks
/// or makes its descriptors while another holds it.
///
/// That is all it orders. A socket made anywhere else at the instant of a
/// fork (a step's stub listener, a reqwest connection, on the poll loop or
/// a runtime worker) can still be inherited on macOS: std's `Command` does
/// not expose `posix_spawn`'s close-every-other-descriptor default, so no
/// lock in this crate closes that window.
///
/// It covers the spawn or the probe alone: never a child's life, a wait or
/// a binary lookup. It is tokio's mutex, so code on the poll loop waits for
/// it without blocking the loop ([`with_spawn_lock_async`]), and a panic
/// while it is held releases it like any other unwind.
static SPAWN_LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

/// Run `spawn`, one process creation or one listener probe, holding the
/// process-wide spawn lock.
///
/// For synchronous code: a closure under [`off_loop`], or a plain
/// `#[test]`. The lock's reasons are on `SPAWN_LOCK`, below this crate's
/// public surface.
///
/// # Panics
///
/// Panics if called from within an asynchronous execution context, such as
/// a task or a `#[tokio::test]` body, as tokio's `blocking_lock` does.
/// Async code takes [`with_spawn_lock_async`].
pub fn with_spawn_lock<T>(spawn: impl FnOnce() -> T) -> T {
    let _held = SPAWN_LOCK.blocking_lock();
    spawn()
}

/// [`with_spawn_lock`] for async code that creates its process where it
/// stands.
///
/// Waits for the lock without blocking the poll loop, then runs `spawn`
/// synchronously while holding it. `OmniSim` is the case that must spawn on
/// the loop (see [`off_loop`] on `PR_SET_PDEATHSIG`).
pub async fn with_spawn_lock_async<T>(spawn: impl FnOnce() -> T) -> T {
    let _held = SPAWN_LOCK.lock().await;
    spawn()
}

/// `N` distinct ephemeral loopback ports, for children that must be told
/// their port because they cannot bind port 0 and print the one they got.
///
/// Binds `127.0.0.1:0` `N` times, holds every listener until all `N` ports
/// are read so no two collide, then lets them go for the children to claim.
/// The ports come from the OS's ephemeral range, so another process can
/// bind one before its child does, and that process can be a server of the
/// child's own kind, which a readiness check asking only "does something
/// answer?" adopts as the child. A caller therefore proves that what
/// answers on each port is its own child, and starts again on fresh ports
/// when it cannot. A child that binds port 0 and prints its port needs
/// none of this ([`parse_bound_port`], [`parse_port_after`]), and
/// [`reserved_test_port_async`] is the other shape, a band below the
/// ephemeral floor probed by connecting, for a port a test must know
/// before the child starts.
///
/// The probe runs under the spawn lock: on some targets a socket is made in
/// two steps, and a fork landing between them would keep a copy of a
/// listener, and so its port, bound after the probe let it go.
///
/// # Panics
///
/// Panics if a listener cannot be bound or read back, or, as
/// [`with_spawn_lock`] does, when called from within an async context,
/// where it runs under [`off_loop`] instead.
#[must_use]
pub fn ephemeral_ports<const N: usize>() -> [u16; N] {
    with_spawn_lock(bind_ephemeral_ports::<N>)
}

fn bind_ephemeral_ports<const N: usize>() -> [u16; N] {
    let listeners: Vec<std::net::TcpListener> = (0..N)
        .map(|_| {
            std::net::TcpListener::bind("127.0.0.1:0")
                .unwrap_or_else(|e| panic!("bind an ephemeral loopback port: {e}"))
        })
        .collect();
    let mut ports = [0u16; N];
    for (port, listener) in ports.iter_mut().zip(&listeners) {
        *port = listener
            .local_addr()
            .unwrap_or_else(|e| panic!("read back an ephemeral port: {e}"))
            .port();
    }
    debug!(?ports, "probed ephemeral ports");
    ports
}

/// Spawn `binary`, feed it `stdin_data`, and reap its `Output`.
fn run_to_completion(
    package_name: &str,
    binary: &str,
    args: &[&str],
    stdin_data: Option<&[u8]>,
) -> std::process::Output {
    let mut cmd = std::process::Command::new(binary);
    cmd.args(args);
    if let Some((key, value)) = child_coverage_profile_var(package_name) {
        cmd.env(key, value);
    }

    if let Some(data) = stdin_data {
        use std::io::Write;

        cmd.stdin(std::process::Stdio::piped());
        cmd.stdout(std::process::Stdio::piped());
        cmd.stderr(std::process::Stdio::piped());

        let mut child = with_spawn_lock(|| cmd.spawn())
            .unwrap_or_else(|e| panic!("failed to spawn {package_name}: {e}"));

        if let Some(ref mut stdin) = child.stdin {
            stdin
                .write_all(data)
                .unwrap_or_else(|e| panic!("failed to write stdin for {package_name}: {e}"));
        }
        drop(child.stdin.take());

        child
            .wait_with_output()
            .unwrap_or_else(|e| panic!("failed to wait on {package_name}: {e}"))
    } else {
        // `output()`'s own stdio, spawned under the lock and waited on
        // outside it.
        cmd.stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped());
        with_spawn_lock(|| cmd.spawn())
            .unwrap_or_else(|e| panic!("failed to run {package_name}: {e}"))
            .wait_with_output()
            .unwrap_or_else(|e| panic!("failed to wait on {package_name}: {e}"))
    }
}

/// Find a pre-built service binary, or return `None`.
///
/// Discovery order:
/// 1. The conventional env var `{PACKAGE_UPPER_SNAKE}_BINARY`
///    (e.g., `FILEMONITOR_BINARY=/path/to/bin`). A set override is used as
///    given, even when nothing is at that path: a stale one then fails the
///    spawn loudly, naming the path, rather than letting the search below
///    quietly test some other build.
/// 2. `$CARGO_TARGET_DIR/debug/<pkg>` (or `$CARGO_TARGET_DIR/$CARGO_BUILD_TARGET/debug/<pkg>`
///    when the latter is set). `CARGO_LLVM_COV_TARGET_DIR` is also honored.
/// 3. Walking up from the current directory, probe `<ancestor>/target/debug/<pkg>` (and
///    the `CARGO_BUILD_TARGET`-qualified variant), stopping at the enclosing cargo
///    workspace root. Cargo's `cargo test -p <pkg>` sets the cwd to the package dir;
///    the workspace `target/` is then one level up.
fn find_binary(package_name: &str) -> Option<String> {
    if let Ok(path) = std::env::var(binary_env_var(package_name)) {
        return Some(path);
    }

    let binary_name = if cfg!(target_os = "windows") {
        format!("{package_name}.exe")
    } else {
        package_name.to_string()
    };
    let triple = std::env::var("CARGO_BUILD_TARGET").ok();

    let candidate = |target_dir: &std::path::Path| -> Option<String> {
        if let Some(triple) = triple.as_deref() {
            let path = target_dir.join(triple).join("debug").join(&binary_name);
            if path.exists() {
                return Some(path.to_string_lossy().into_owned());
            }
        }
        let path = target_dir.join("debug").join(&binary_name);
        if path.exists() {
            return Some(path.to_string_lossy().into_owned());
        }
        None
    };

    // When `CARGO_TARGET_DIR` (or `CARGO_LLVM_COV_TARGET_DIR`) is set, honor
    // it exclusively. Walking up afterwards could silently pick up a stale
    // non-instrumented binary at `target/debug/<pkg>` and skip coverage
    // data collection for it.
    if let Some(dir) = std::env::var_os("CARGO_TARGET_DIR")
        .or_else(|| std::env::var_os("CARGO_LLVM_COV_TARGET_DIR"))
    {
        return candidate(std::path::Path::new(&dir));
    }

    if let Ok(cwd) = std::env::current_dir() {
        for ancestor in cwd.ancestors() {
            if let Some(path) = candidate(&ancestor.join("target")) {
                return Some(path);
            }
            // Probe the workspace root, then stop: never climb into an
            // enclosing checkout (see [`is_workspace_root`]).
            if is_workspace_root(ancestor) {
                break;
            }
        }
    }

    None
}

/// Whether `dir` is a cargo workspace root — a `Cargo.toml` carrying a
/// `[workspace]` table.
///
/// The ancestor walk stops here so a checkout nested inside another cannot
/// resolve a binary built from the **outer** one. A git worktree under
/// `<repo>/.claude/worktrees/<name>` is the case that motivates it: with no
/// binary built inside the worktree, the walk used to climb out and spawn
/// the main checkout's `target/debug/<pkg>`, which is stale by however long
/// ago that checkout was last built. The suite then passes green while
/// exercising a binary built from another branch entirely — the failure mode
/// a test run cannot report, because nothing errors. Stopping here turns it
/// into [`require_binary`]'s panic, which names the build command to run.
fn is_workspace_root(dir: &std::path::Path) -> bool {
    std::fs::read_to_string(dir.join("Cargo.toml")).is_ok_and(|manifest| {
        manifest
            .lines()
            .any(|line| line.trim_start().starts_with("[workspace]"))
    })
}

/// [`find_binary`] or panic with a diagnostic pointing the user at the fix.
fn require_binary(package_name: &str) -> String {
    find_binary(package_name).unwrap_or_else(|| {
        panic!(
            "bdd-infra: binary for package `{pkg}` not found. \
             BDD tests require a pre-built binary — run \
             `cargo build -p {pkg} --all-features` (or `cargo build --all-features --all-targets` \
             for the whole workspace), or set `{env}` to an explicit binary path.",
            pkg = package_name,
            env = binary_env_var(package_name),
        )
    })
}

/// Windows process creation flag: place the child in its own process group so
/// that `CTRL_BREAK_EVENT` can target it without affecting the test runner.
#[cfg(windows)]
const CREATE_NEW_PROCESS_GROUP: u32 = 0x0000_0200;

/// The service's command line, built on the poll loop: nothing here
/// blocks.
///
/// On Windows the child is spawned with [`CREATE_NEW_PROCESS_GROUP`] so that
/// [`send_sigterm`] can deliver `CTRL_BREAK_EVENT` only to the child's group
/// without affecting the test runner.
fn service_command(
    binary: &str,
    package_name: &str,
    args: &[&str],
    envs: &[(&str, &str)],
) -> tokio::process::Command {
    debug!(binary = %binary, ?args, "starting {} from pre-built binary", package_name);
    let mut cmd = tokio::process::Command::new(binary);
    cmd.args(args)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    for (key, value) in envs {
        cmd.env(key, value);
    }
    if let Some((key, value)) = child_coverage_profile_var(package_name) {
        debug!(profile = ?value, "routing {} child coverage into COVERAGE_DIR", package_name);
        cmd.env(key, value);
    }
    #[cfg(windows)]
    {
        cmd.creation_flags(CREATE_NEW_PROCESS_GROUP);
    }
    cmd
}

/// Find the service's binary and spawn it, both off the poll loop: the
/// lookup stats files and reads `Cargo.toml`, and creating a process is a
/// synchronous call a loaded host makes slow.
async fn spawn_service(
    package_name: &str,
    args: &[&str],
    envs: &[(&str, &str)],
) -> tokio::process::Child {
    let package = package_name.to_string();
    let binary = off_loop(move || require_binary(&package)).await;
    let mut cmd = service_command(&binary, package_name, args, envs);
    let package = package_name.to_string();
    off_loop(move || {
        with_spawn_lock(|| cmd.spawn())
            .unwrap_or_else(|e| panic!("failed to start {package} binary '{binary}': {e}"))
    })
    .await
}

/// Forward a spawned child's stderr to this process's own stderr line by
/// line, each line prefixed with `label` (a `<package>#<seq>` tag from
/// [`next_spawn_label`]).
///
/// Without this, a child's `tracing` output — which every service writes to
/// stderr — would go straight to this process's inherited stderr with zero
/// attribution. Under cucumber's default concurrency, many same-named
/// service instances (spawned by different concurrently-running scenarios)
/// can be alive at once, so their lines interleave indistinguishably in
/// captured BDD/CI output (issue #578). The label lets a reader (or a `grep`)
/// isolate one instance's lines from the merged stream.
///
/// Lines are passed through verbatim — this only adds a prefix, it never
/// reformats or interprets the child's own (already fully-formatted, often
/// ANSI-colored) log line — so a plain `eprint!` is used rather than routing
/// through this crate's own `tracing` macros, which would wrap it a second
/// time.
fn spawn_stderr_forwarder(
    stderr: tokio::process::ChildStderr,
    label: String,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let mut reader = BufReader::new(stderr);
        let mut line = String::new();
        loop {
            line.clear();
            match reader.read_line(&mut line).await {
                Ok(0) | Err(_) => break,
                Ok(_) => eprint!("[{label}] {line}"),
            }
        }
    })
}

/// Join a background task, giving it 1s to finish before aborting it.
///
/// Called only after the child it was draining/forwarding for has already
/// exited, so the task's pipe has hit EOF and it should return almost
/// immediately; the abort is a belt-and-braces guard against the join
/// hanging, not the expected path.
async fn join_or_abort(mut handle: tokio::task::JoinHandle<()>) {
    if tokio::time::timeout(Duration::from_secs(1), &mut handle)
        .await
        .is_err()
    {
        handle.abort();
    }
}

/// Parse the bound port from a service's stdout.
///
/// Scans each line for `bound_addr=<host>:<port>` and extracts the port.
/// After finding it, spawns a background task to drain remaining stdout so
/// the service process never blocks on a full pipe buffer.
///
/// This is a universal parser — it works regardless of what human-readable
/// text precedes `bound_addr=` in the output line.
pub async fn parse_bound_port(
    stdout: tokio::process::ChildStdout,
) -> Option<(u16, tokio::task::JoinHandle<()>)> {
    parse_port_after(stdout, "bound_addr=").await
}

/// Parse the bound port from the stdout of a child that announces its
/// address after `marker`.
///
/// [`parse_bound_port`] is this for our services' `bound_addr=`; this
/// form is for a third-party binary told to bind port 0 that prints the
/// address it got under its own wording, such as geckodriver's
/// `Listening on `. The text after `marker` on the first line carrying it
/// ends in `:<port>`.
///
/// Returns `None` at end of output without a match, and otherwise drains
/// the rest of stdout in a background task, as [`parse_bound_port`] does.
pub async fn parse_port_after(
    stdout: tokio::process::ChildStdout,
    marker: &str,
) -> Option<(u16, tokio::task::JoinHandle<()>)> {
    let mut reader = BufReader::new(stdout);
    let mut line = String::new();

    while reader.read_line(&mut line).await.ok()? > 0 {
        if let Some((_, after)) = line.split_once(marker) {
            let addr_str = after.trim();
            if let Some(port_str) = addr_str.split(':').next_back() {
                if let Ok(port) = port_str.parse::<u16>() {
                    let drain_handle = tokio::spawn(async move {
                        let mut buf = String::new();
                        while reader.read_line(&mut buf).await.unwrap_or(0) > 0 {
                            buf.clear();
                        }
                    });
                    return Some((port, drain_handle));
                }
            }
        }
        line.clear();
    }
    None
}

/// What [`send_sigterm`] actually delivers, and what the timeout escalates to.
/// Named per platform so a shutdown log line points a reader at the mechanism
/// that ran rather than at its Unix spelling.
#[cfg(unix)]
const GRACEFUL_EVENT: &str = "SIGTERM";
#[cfg(unix)]
const FORCED_STOP: &str = "SIGKILL";
#[cfg(windows)]
const GRACEFUL_EVENT: &str = "CTRL_BREAK_EVENT";
#[cfg(windows)]
const FORCED_STOP: &str = "TerminateProcess";
#[cfg(not(any(unix, windows)))]
compile_error!(
    "rusty-photon supports unix and windows targets only; please open a GitHub issue at \
     https://github.com/rusty-photon/rusty-photon/issues naming the platform you need"
);

/// Send a graceful-shutdown signal to a process.
///
/// * **Unix** — sends `SIGTERM`.
/// * **Windows** — sends `CTRL_BREAK_EVENT` via `GenerateConsoleCtrlEvent`.
///   The child must have been spawned with `CREATE_NEW_PROCESS_GROUP` so the
///   event targets only its process group (see [`service_command`]).
fn send_sigterm(pid: u32) {
    #[cfg(unix)]
    {
        // SAFETY: libc::kill with a valid pid and SIGTERM is safe.
        let ret = unsafe { libc::kill(pid.cast_signed(), libc::SIGTERM) };
        if ret != 0 {
            // errno is read here, not inside the macro: a log field runs only
            // once the callsite is known to be enabled, which puts the
            // subscriber's own work between the failing call and the read.
            let err = std::io::Error::last_os_error();
            debug!("failed to send SIGTERM to pid {pid}: {err}");
        }
    }
    #[cfg(windows)]
    {
        // SAFETY: GenerateConsoleCtrlEvent with CTRL_BREAK_EVENT and a valid
        // process-group id is the documented way to request graceful shutdown
        // of a console process on Windows.
        #[allow(non_snake_case)]
        extern "system" {
            fn GenerateConsoleCtrlEvent(dw_ctrl_event: u32, dw_process_group_id: u32) -> i32;
        }
        const CTRL_BREAK_EVENT: u32 = 1;
        let ret = unsafe { GenerateConsoleCtrlEvent(CTRL_BREAK_EVENT, pid) };
        if ret == 0 {
            // Read before the macro, for the reason given on the Unix arm.
            let err = std::io::Error::last_os_error();
            debug!("failed to send CTRL_BREAK_EVENT to process group {pid}: {err}");
        }
    }
}

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
#[allow(clippy::expect_used)]
mod tests {
    use super::*;
    use std::process::Stdio;

    /// Guard for tests that call `set_current_dir`. cargo-nextest runs each
    /// test in its own process so this is a no-op there, but `cargo test`
    /// (used by the coverage job) runs tests as threads in a single process.
    /// The mutex serializes cwd-changing tests so they don't stomp each other.
    static CWD_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    /// Spawn `program` with `args`, stdout and stderr piped, under the spawn
    /// lock: these children's pipes are read to EOF beside tests that start
    /// long-lived services.
    async fn spawn_piped(program: &str, args: &[&str]) -> tokio::process::Child {
        let mut cmd = tokio::process::Command::new(program);
        cmd.args(args).stdout(Stdio::piped()).stderr(Stdio::piped());
        with_spawn_lock_async(|| cmd.spawn())
            .await
            .unwrap_or_else(|e| panic!("spawn {program}: {e}"))
    }

    // -----------------------------------------------------------------------
    // parse_bound_port tests
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn test_parse_bound_port_alpaca_prefix() {
        let mut child =
            spawn_piped("echo", &["Bound Alpaca server bound_addr=0.0.0.0:54321"]).await;
        let stdout = child.stdout.take().unwrap();

        let (port, drain) = parse_bound_port(stdout).await.unwrap();
        assert_eq!(port, 54321);
        drain.abort();
    }

    #[tokio::test]
    async fn test_parse_bound_port_rp_prefix() {
        let mut child = spawn_piped("echo", &["Bound rp server bound_addr=127.0.0.1:9999"]).await;
        let stdout = child.stdout.take().unwrap();

        let (port, drain) = parse_bound_port(stdout).await.unwrap();
        assert_eq!(port, 9999);
        drain.abort();
    }

    #[tokio::test]
    async fn test_parse_bound_port_arbitrary_prefix() {
        let mut child =
            spawn_piped("echo", &["some future service bound_addr=10.0.0.1:8080"]).await;
        let stdout = child.stdout.take().unwrap();

        let (port, drain) = parse_bound_port(stdout).await.unwrap();
        assert_eq!(port, 8080);
        drain.abort();
    }

    #[tokio::test]
    async fn test_parse_bound_port_with_preceding_lines() {
        // printf outputs multiple lines; the port line comes after noise
        let mut child = spawn_piped(
            "printf",
            &["starting up...\nloading config\nBound Alpaca server bound_addr=0.0.0.0:11111\n"],
        )
        .await;
        let stdout = child.stdout.take().unwrap();

        let (port, drain) = parse_bound_port(stdout).await.unwrap();
        assert_eq!(port, 11111);
        drain.abort();
    }

    #[tokio::test]
    async fn test_parse_bound_port_no_match_returns_none() {
        let mut child = spawn_piped("echo", &["no port info here"]).await;
        let stdout = child.stdout.take().unwrap();

        let result = parse_bound_port(stdout).await;
        assert!(result.is_none());
    }

    #[tokio::test]
    async fn test_parse_bound_port_empty_output_returns_none() {
        let mut child = spawn_piped("true", &[]).await;
        let stdout = child.stdout.take().unwrap();

        let result = parse_bound_port(stdout).await;
        assert!(result.is_none());
    }

    #[tokio::test]
    async fn test_parse_port_after_reads_a_third_party_marker() {
        // geckodriver's announcement, tab-separated after a millisecond clock.
        let mut child = spawn_piped(
            "printf",
            &["1792324339384\tgeckodriver\tINFO\tListening on 127.0.0.1:40861\n"],
        )
        .await;
        let stdout = child.stdout.take().unwrap();

        let (port, drain) = parse_port_after(stdout, "Listening on ").await.unwrap();
        assert_eq!(port, 40861);
        drain.abort();
    }

    #[tokio::test]
    async fn test_parse_port_after_ignores_another_marker() {
        let mut child = spawn_piped("echo", &["Bound rp server bound_addr=127.0.0.1:9999"]).await;
        let stdout = child.stdout.take().unwrap();

        let result = parse_port_after(stdout, "Listening on ").await;
        assert!(result.is_none());
    }

    // -----------------------------------------------------------------------
    // next_spawn_label / spawn_stderr_forwarder tests (issue #578: label
    // every spawned child so concurrently-running same-named instances'
    // stderr can be told apart in merged BDD/CI output)
    // -----------------------------------------------------------------------

    #[test]
    fn test_next_spawn_label_is_unique_per_call_with_package_prefix() {
        let a = next_spawn_label("widget");
        let b = next_spawn_label("widget");
        assert_ne!(
            a, b,
            "two spawns of the same package must get distinct labels"
        );
        assert!(a.starts_with("widget#"), "{a}");
        assert!(b.starts_with("widget#"), "{b}");
    }

    #[tokio::test]
    async fn test_spawn_stderr_forwarder_terminates_on_eof() {
        let mut child = spawn_piped("sh", &["-c", "echo one >&2; echo two >&2"]).await;
        let stderr = child.stderr.take().unwrap();
        let handle = spawn_stderr_forwarder(stderr, "test#0".to_string());

        // The child's stderr write end closes once it exits; the forwarder
        // must observe EOF and return rather than block forever.
        tokio::time::timeout(std::time::Duration::from_secs(5), handle)
            .await
            .expect("forwarder did not terminate after the child's stderr closed")
            .unwrap();
    }

    // -----------------------------------------------------------------------
    // __bdd_bazel_chdir tests
    //
    // All tests that call set_current_dir hold CWD_LOCK to prevent
    // interference when `cargo test` runs them as threads (coverage job).
    // -----------------------------------------------------------------------

    #[test]
    fn test_bdd_bazel_chdir_noop_when_env_unset() {
        let _lock = CWD_LOCK.lock().unwrap();
        std::env::remove_var("BDD_PACKAGE_DIR");
        let before = std::env::current_dir().unwrap();
        __bdd_bazel_chdir();
        assert_eq!(std::env::current_dir().unwrap(), before);
    }

    #[test]
    fn test_bdd_bazel_chdir_changes_directory() {
        let _lock = CWD_LOCK.lock().unwrap();
        let tmp = tempfile::tempdir().unwrap();
        let target = tmp.path().join("subdir");
        std::fs::create_dir_all(&target).unwrap();

        let previous = std::env::current_dir().unwrap();
        std::env::set_var("BDD_PACKAGE_DIR", &target);
        __bdd_bazel_chdir();
        let after = std::env::current_dir().unwrap();
        std::env::set_current_dir(&previous).unwrap();
        std::env::remove_var("BDD_PACKAGE_DIR");

        // Canonicalize both sides: on macOS /var → /private/var.
        assert_eq!(
            after.canonicalize().unwrap(),
            target.canonicalize().unwrap()
        );
    }

    #[test]
    fn test_bdd_bazel_chdir_absolutizes_binary_env_vars() {
        let _lock = CWD_LOCK.lock().unwrap();
        let tmp = tempfile::tempdir().unwrap();
        let target = tmp.path().join("pkg");
        std::fs::create_dir_all(&target).unwrap();

        let previous = std::env::current_dir().unwrap();
        let unique_var = "TEST_CHDIR_ABS_BINARY";
        std::env::set_var(unique_var, "relative/path/to/bin");
        std::env::set_var("BDD_PACKAGE_DIR", &target);

        __bdd_bazel_chdir();

        let absolutized = std::env::var(unique_var).unwrap();
        std::env::set_current_dir(&previous).unwrap();
        std::env::remove_var("BDD_PACKAGE_DIR");
        std::env::remove_var(unique_var);

        assert_eq!(
            std::path::PathBuf::from(&absolutized),
            previous.join("relative/path/to/bin")
        );
    }

    #[test]
    fn test_bdd_bazel_chdir_skips_absolute_binary_env_vars() {
        let _lock = CWD_LOCK.lock().unwrap();
        let tmp = tempfile::tempdir().unwrap();
        let target = tmp.path().join("pkg");
        std::fs::create_dir_all(&target).unwrap();

        // Use the tempdir's own path as the absolute binary value — it's
        // guaranteed absolute on every platform (including the correct
        // drive letter on Windows).
        let abs_path = tmp.path().join("bin").to_string_lossy().to_string();

        let previous = std::env::current_dir().unwrap();
        let unique_var = "TEST_CHDIR_SKIP_BINARY";
        std::env::set_var(unique_var, &abs_path);
        std::env::set_var("BDD_PACKAGE_DIR", &target);

        __bdd_bazel_chdir();

        let value = std::env::var(unique_var).unwrap();
        std::env::set_current_dir(&previous).unwrap();
        std::env::remove_var("BDD_PACKAGE_DIR");
        std::env::remove_var(unique_var);

        assert_eq!(value, abs_path);
    }

    #[test]
    fn test_bdd_bazel_chdir_ignores_non_binary_env_vars() {
        let _lock = CWD_LOCK.lock().unwrap();
        let tmp = tempfile::tempdir().unwrap();
        let target = tmp.path().join("pkg");
        std::fs::create_dir_all(&target).unwrap();

        let previous = std::env::current_dir().unwrap();
        let unique_var = "TEST_CHDIR_NOT_A_BINARY_SUFFIX";
        std::env::set_var(unique_var, "relative/path");
        std::env::set_var("BDD_PACKAGE_DIR", &target);

        __bdd_bazel_chdir();

        let value = std::env::var(unique_var).unwrap();
        std::env::set_current_dir(&previous).unwrap();
        std::env::remove_var("BDD_PACKAGE_DIR");
        std::env::remove_var(unique_var);

        // Var does NOT end with _BINARY, so it should not be absolutized.
        assert_eq!(value, "relative/path");
    }

    #[test]
    fn test_bdd_bazel_chdir_absolutizes_coverage_dir() {
        let _lock = CWD_LOCK.lock().unwrap();
        let tmp = tempfile::tempdir().unwrap();
        let target = tmp.path().join("pkg");
        std::fs::create_dir_all(&target).unwrap();

        let previous = std::env::current_dir().unwrap();
        let saved_cov = std::env::var_os("COVERAGE_DIR");
        std::env::set_var("COVERAGE_DIR", "relative/cov/dir");
        std::env::set_var("BDD_PACKAGE_DIR", &target);

        __bdd_bazel_chdir();

        let absolutized = std::env::var("COVERAGE_DIR").unwrap();
        std::env::set_current_dir(&previous).unwrap();
        std::env::remove_var("BDD_PACKAGE_DIR");
        match saved_cov {
            Some(v) => std::env::set_var("COVERAGE_DIR", v),
            None => std::env::remove_var("COVERAGE_DIR"),
        }

        // COVERAGE_DIR is absolutized like the *_BINARY vars so a spawned
        // child's LLVM_PROFILE_FILE still resolves after the chdir.
        assert_eq!(
            std::path::PathBuf::from(&absolutized),
            previous.join("relative/cov/dir")
        );
    }

    // -----------------------------------------------------------------------
    // binary_env_var tests
    // -----------------------------------------------------------------------

    #[test]
    fn test_binary_env_var_uppercases_and_replaces_dashes() {
        assert_eq!(binary_env_var("rp"), "RP_BINARY");
        assert_eq!(binary_env_var("ppba-driver"), "PPBA_DRIVER_BINARY");
        assert_eq!(binary_env_var("qhy-focuser"), "QHY_FOCUSER_BINARY");
    }

    // -----------------------------------------------------------------------
    // child_coverage_profile_path tests
    // -----------------------------------------------------------------------

    #[test]
    fn test_child_coverage_profile_path_joins_dir_and_llvm_pattern() {
        let path = child_coverage_profile_path(std::ffi::OsStr::new("/cov/dir"), "rp");
        let path = std::path::PathBuf::from(path);
        // Per-service online-merge pool inside COVERAGE_DIR; %8m bounds the file
        // count (no %p, which would make one file per child process).
        assert_eq!(
            path.file_name().unwrap(),
            std::ffi::OsStr::new("rp-%8m.profraw")
        );
        assert_eq!(path.parent().unwrap(), std::path::Path::new("/cov/dir"));
    }

    #[test]
    fn test_child_coverage_profile_path_preserves_dashed_package_name() {
        let path = child_coverage_profile_path(std::ffi::OsStr::new("/c"), "ppba-driver");
        let path = std::path::PathBuf::from(path);
        assert_eq!(
            path.file_name().unwrap(),
            std::ffi::OsStr::new("ppba-driver-%8m.profraw")
        );
    }

    #[test]
    fn test_child_coverage_profile_var_gates_on_coverage_dir() {
        // The COVERAGE_DIR gate is the invariant that keeps the cargo /
        // cargo-llvm-cov and plain `bazel test` paths untouched: no
        // COVERAGE_DIR => no child env override. CWD_LOCK serialises the
        // env mutation against the other COVERAGE_DIR-touching test.
        let _lock = CWD_LOCK.lock().unwrap();
        let saved = std::env::var_os("COVERAGE_DIR");

        std::env::remove_var("COVERAGE_DIR");
        let unset = child_coverage_profile_var("rp");

        std::env::set_var("COVERAGE_DIR", "/cov/dir");
        let set = child_coverage_profile_var("rp");

        match saved {
            Some(v) => std::env::set_var("COVERAGE_DIR", v),
            None => std::env::remove_var("COVERAGE_DIR"),
        }

        assert!(
            unset.is_none(),
            "without COVERAGE_DIR the child env must be left untouched"
        );
        let (key, value) = set.expect("COVERAGE_DIR set => LLVM_PROFILE_FILE override");
        assert_eq!(key, "LLVM_PROFILE_FILE");
        let value = std::path::PathBuf::from(value);
        assert_eq!(value.parent().unwrap(), std::path::Path::new("/cov/dir"));
        assert_eq!(
            value.file_name().unwrap(),
            std::ffi::OsStr::new("rp-%8m.profraw")
        );
    }

    // -----------------------------------------------------------------------
    // find_binary tests
    //
    // Grouped into a submodule so `:bdd-infra_binary_discovery_unit_test` can
    // select them by module path: the enclosing `:bdd-infra_unit_test` is
    // tagged requires-cargo for the run_once_* tests below, which would
    // otherwise keep every test in `tests::` out of per-PR Bazel runs and out
    // of coverage. Same shape as `:bdd-infra_signals_unit_test`.
    // -----------------------------------------------------------------------

    mod binary_discovery {
        use super::CWD_LOCK;
        use crate::{find_binary, is_workspace_root, require_binary};

        #[test]
        fn test_find_binary_from_env_var() {
            // Use a package name whose derived env var is unique to this test.
            let package = "bdd-infra-test-find-env";
            std::env::set_var("BDD_INFRA_TEST_FIND_ENV_BINARY", "/some/path/to/binary");
            let result = find_binary(package);
            std::env::remove_var("BDD_INFRA_TEST_FIND_ENV_BINARY");

            assert_eq!(result, Some("/some/path/to/binary".to_string()));
        }

        #[test]
        fn test_find_binary_returns_none_when_nothing_found() {
            let package = "bdd-infra-test-find-none";
            std::env::remove_var("BDD_INFRA_TEST_FIND_NONE_BINARY");
            let result = find_binary(package);
            assert!(result.is_none());
        }

        #[test]
        fn test_find_binary_in_target_dir() {
            // Mutates CARGO_TARGET_DIR; serialize with sibling tests that read
            // it (e.g. `ensure_rp_binary`) via CWD_LOCK.
            let _lock = CWD_LOCK.lock().unwrap();

            let dir = tempfile::tempdir().unwrap();
            let debug_dir = dir.path().join("debug");
            std::fs::create_dir_all(&debug_dir).unwrap();

            let binary_name = if cfg!(target_os = "windows") {
                "my-service.exe"
            } else {
                "my-service"
            };
            let binary_path = debug_dir.join(binary_name);
            std::fs::write(&binary_path, "fake binary").unwrap();

            // Make sure the derived env var isn't set, so we exercise the
            // target-dir branch.
            std::env::remove_var("MY_SERVICE_BINARY");
            let old_target = std::env::var("CARGO_TARGET_DIR").ok();
            std::env::set_var("CARGO_TARGET_DIR", dir.path());

            let result = find_binary("my-service");

            match old_target {
                Some(v) => std::env::set_var("CARGO_TARGET_DIR", v),
                None => std::env::remove_var("CARGO_TARGET_DIR"),
            }

            // Compare as PathBuf so we don't trip on Windows' mixed separators
            // (Path::join produces `C:\…\debug\my-service.exe`, but a
            // `format!("{}/debug/…")` expected string would have a forward
            // slash in the suffix).
            assert_eq!(
                result.map(std::path::PathBuf::from),
                Some(debug_dir.join(binary_name))
            );
        }

        /// Covers the `CARGO_BUILD_TARGET` triple branch of the `candidate`
        /// closure in `find_binary`.
        #[test]
        fn test_find_binary_in_target_dir_with_triple() {
            // CARGO_BUILD_TARGET is process-global; serialize with other tests
            // that mutate cwd/env via CWD_LOCK so concurrent test threads don't
            // stomp each other under `cargo test` (coverage job).
            let _lock = CWD_LOCK.lock().unwrap();

            let dir = tempfile::tempdir().unwrap();
            let triple = "x86_64-unknown-linux-gnu";
            let debug_dir = dir.path().join(triple).join("debug");
            std::fs::create_dir_all(&debug_dir).unwrap();

            let binary_name = if cfg!(target_os = "windows") {
                "bdd-infra-test-triple.exe"
            } else {
                "bdd-infra-test-triple"
            };
            let binary_path = debug_dir.join(binary_name);
            std::fs::write(&binary_path, "fake binary").unwrap();

            let old_target = std::env::var("CARGO_TARGET_DIR").ok();
            let old_triple = std::env::var("CARGO_BUILD_TARGET").ok();
            std::env::remove_var("BDD_INFRA_TEST_TRIPLE_BINARY");
            std::env::set_var("CARGO_TARGET_DIR", dir.path());
            std::env::set_var("CARGO_BUILD_TARGET", triple);

            let result = find_binary("bdd-infra-test-triple");

            match old_target {
                Some(v) => std::env::set_var("CARGO_TARGET_DIR", v),
                None => std::env::remove_var("CARGO_TARGET_DIR"),
            }
            match old_triple {
                Some(v) => std::env::set_var("CARGO_BUILD_TARGET", v),
                None => std::env::remove_var("CARGO_BUILD_TARGET"),
            }

            assert_eq!(
                result.map(std::path::PathBuf::from),
                Some(debug_dir.join(binary_name))
            );
        }

        /// Covers the cwd-ancestors walk branch of `find_binary` — the path taken
        /// when no `CARGO_TARGET_DIR` / `CARGO_LLVM_COV_TARGET_DIR` is set, which
        /// is how `cargo test -p <pkg>` from a package directory finds the
        /// workspace `target/`.
        #[test]
        fn test_find_binary_via_ancestor_walk() {
            let _lock = CWD_LOCK.lock().unwrap();

            let dir = tempfile::tempdir().unwrap();
            let debug_dir = dir.path().join("target").join("debug");
            std::fs::create_dir_all(&debug_dir).unwrap();

            let binary_name = if cfg!(target_os = "windows") {
                "bdd-infra-test-walk.exe"
            } else {
                "bdd-infra-test-walk"
            };
            let binary_path = debug_dir.join(binary_name);
            std::fs::write(&binary_path, "fake binary").unwrap();

            let subdir = dir.path().join("pkg");
            std::fs::create_dir_all(&subdir).unwrap();

            let previous = std::env::current_dir().unwrap();
            let old_target = std::env::var("CARGO_TARGET_DIR").ok();
            let old_llvm_cov = std::env::var("CARGO_LLVM_COV_TARGET_DIR").ok();
            let old_triple = std::env::var("CARGO_BUILD_TARGET").ok();
            std::env::remove_var("BDD_INFRA_TEST_WALK_BINARY");
            std::env::remove_var("CARGO_TARGET_DIR");
            std::env::remove_var("CARGO_LLVM_COV_TARGET_DIR");
            std::env::remove_var("CARGO_BUILD_TARGET");
            std::env::set_current_dir(&subdir).unwrap();

            let result = find_binary("bdd-infra-test-walk");

            std::env::set_current_dir(&previous).unwrap();
            match old_target {
                Some(v) => std::env::set_var("CARGO_TARGET_DIR", v),
                None => std::env::remove_var("CARGO_TARGET_DIR"),
            }
            match old_llvm_cov {
                Some(v) => std::env::set_var("CARGO_LLVM_COV_TARGET_DIR", v),
                None => std::env::remove_var("CARGO_LLVM_COV_TARGET_DIR"),
            }
            match old_triple {
                Some(v) => std::env::set_var("CARGO_BUILD_TARGET", v),
                None => std::env::remove_var("CARGO_BUILD_TARGET"),
            }

            // Canonicalize both sides: on macOS /var → /private/var.
            assert_eq!(
                result
                    .map(std::path::PathBuf::from)
                    .map(|p| p.canonicalize().unwrap()),
                Some(binary_path.canonicalize().unwrap())
            );
        }

        /// A checkout nested inside another must not resolve the **outer**
        /// checkout's binary. The motivating layout is a git worktree at
        /// `<repo>/.claude/worktrees/<name>`: before the workspace-root stop, a
        /// worktree with nothing built inside it spawned the main checkout's
        /// `target/debug/<pkg>` — a binary from another branch, arbitrarily
        /// stale — and the suite passed green against it.
        #[test]
        fn test_find_binary_does_not_escape_into_an_enclosing_checkout() {
            let _lock = CWD_LOCK.lock().unwrap();

            let binary_name = if cfg!(target_os = "windows") {
                "bdd-infra-test-nested.exe"
            } else {
                "bdd-infra-test-nested"
            };

            // Outer checkout: a workspace with the binary built.
            let outer = tempfile::tempdir().unwrap();
            std::fs::write(
                outer.path().join("Cargo.toml"),
                "[workspace]\nmembers = []\n",
            )
            .unwrap();
            let outer_debug = outer.path().join("target").join("debug");
            std::fs::create_dir_all(&outer_debug).unwrap();
            std::fs::write(outer_debug.join(binary_name), "outer binary").unwrap();

            // Inner checkout: its own workspace, nothing built.
            let inner_pkg = outer.path().join(".claude/worktrees/wt/pkg");
            std::fs::create_dir_all(&inner_pkg).unwrap();
            std::fs::write(
                outer.path().join(".claude/worktrees/wt/Cargo.toml"),
                "[workspace]\nmembers = []\n",
            )
            .unwrap();

            let previous = std::env::current_dir().unwrap();
            let saved = TargetEnv::take();
            std::env::remove_var("BDD_INFRA_TEST_NESTED_BINARY");
            std::env::set_current_dir(&inner_pkg).unwrap();

            let result = find_binary("bdd-infra-test-nested");

            std::env::set_current_dir(&previous).unwrap();
            saved.restore();

            assert_eq!(
                result, None,
                "walk escaped the inner workspace and found the outer checkout's binary"
            );
        }

        /// The stop is *after* probing, so a workspace root's own `target/` is
        /// still found — the ordinary `cargo test -p <pkg>` case.
        #[test]
        fn test_find_binary_probes_the_workspace_root_before_stopping() {
            let _lock = CWD_LOCK.lock().unwrap();

            let binary_name = if cfg!(target_os = "windows") {
                "bdd-infra-test-wsroot.exe"
            } else {
                "bdd-infra-test-wsroot"
            };

            let root = tempfile::tempdir().unwrap();
            std::fs::write(
                root.path().join("Cargo.toml"),
                "[workspace]\nmembers = []\n",
            )
            .unwrap();
            let debug_dir = root.path().join("target").join("debug");
            std::fs::create_dir_all(&debug_dir).unwrap();
            let binary_path = debug_dir.join(binary_name);
            std::fs::write(&binary_path, "fake binary").unwrap();

            let pkg = root.path().join("services").join("pkg");
            std::fs::create_dir_all(&pkg).unwrap();

            let previous = std::env::current_dir().unwrap();
            let saved = TargetEnv::take();
            std::env::remove_var("BDD_INFRA_TEST_WSROOT_BINARY");
            std::env::set_current_dir(&pkg).unwrap();

            let result = find_binary("bdd-infra-test-wsroot");

            std::env::set_current_dir(&previous).unwrap();
            saved.restore();

            // Canonicalize both sides: on macOS /var -> /private/var.
            assert_eq!(
                result
                    .map(std::path::PathBuf::from)
                    .map(|p| p.canonicalize().unwrap()),
                Some(binary_path.canonicalize().unwrap())
            );
        }

        /// Inheriting workspace deps does not make a crate a workspace root —
        /// only the `[workspace]` header does. Otherwise a member crate would
        /// stop the walk one level below the real root, where no `target/`
        /// sits.
        #[test]
        fn test_a_member_crate_inheriting_workspace_deps_is_not_a_workspace_root() {
            let dir = tempfile::tempdir().unwrap();
            std::fs::write(
                dir.path().join("Cargo.toml"),
                "[package]\nname = \"member\"\n\n[dependencies]\nserde = { workspace = true }\n",
            )
            .unwrap();
            assert!(!is_workspace_root(dir.path()));
        }

        /// A dotted `[workspace.*]` subtable neither creates a false positive
        /// on its own — `starts_with("[workspace]")` requires the closing
        /// bracket, so `[workspace.dependencies]` does not match — nor hides a
        /// real root that also carries such subtables. That second half is
        /// this repo's own manifest, which pairs `[workspace]` with
        /// `[workspace.package]`, `[workspace.lints.*]` and
        /// `[workspace.dependencies]`.
        #[test]
        fn test_dotted_workspace_subtables_neither_forge_nor_hide_a_root() {
            let subtables_only = tempfile::tempdir().unwrap();
            std::fs::write(
                subtables_only.path().join("Cargo.toml"),
                "[package]\nname = \"member\"\n\n[workspace.dependencies]\nserde = \"1\"\n",
            )
            .unwrap();
            assert!(!is_workspace_root(subtables_only.path()));

            let real_root = tempfile::tempdir().unwrap();
            std::fs::write(
                real_root.path().join("Cargo.toml"),
                "[workspace]\nmembers = []\n\n[workspace.package]\nedition = \"2021\"\n\n\
                 [workspace.dependencies]\nserde = \"1\"\n",
            )
            .unwrap();
            assert!(is_workspace_root(real_root.path()));
        }

        #[test]
        fn test_a_directory_without_a_manifest_is_not_a_workspace_root() {
            let dir = tempfile::tempdir().unwrap();
            assert!(!is_workspace_root(dir.path()));
        }

        /// The cargo target-dir env vars these tests clobber, saved and restored
        /// as a unit so each test's teardown is one line rather than three
        /// match arms.
        struct TargetEnv {
            target_dir: Option<String>,
            llvm_cov_target_dir: Option<String>,
            build_target: Option<String>,
        }

        impl TargetEnv {
            /// Snapshot the three vars and clear them.
            fn take() -> Self {
                let saved = Self {
                    target_dir: std::env::var("CARGO_TARGET_DIR").ok(),
                    llvm_cov_target_dir: std::env::var("CARGO_LLVM_COV_TARGET_DIR").ok(),
                    build_target: std::env::var("CARGO_BUILD_TARGET").ok(),
                };
                std::env::remove_var("CARGO_TARGET_DIR");
                std::env::remove_var("CARGO_LLVM_COV_TARGET_DIR");
                std::env::remove_var("CARGO_BUILD_TARGET");
                saved
            }

            fn restore(self) {
                for (key, value) in [
                    ("CARGO_TARGET_DIR", self.target_dir),
                    ("CARGO_LLVM_COV_TARGET_DIR", self.llvm_cov_target_dir),
                    ("CARGO_BUILD_TARGET", self.build_target),
                ] {
                    match value {
                        Some(v) => std::env::set_var(key, v),
                        None => std::env::remove_var(key),
                    }
                }
            }
        }

        #[test]
        #[should_panic(expected = "binary for package `bdd-infra-test-require-missing` not found")]
        fn test_require_binary_panics_with_diagnostic() {
            // Mutates CARGO_TARGET_DIR; serialize with sibling tests via CWD_LOCK.
            // `catch_unwind` below requires the lock to be released before the
            // panic propagates, so we drop it explicitly at the end.
            let lock = CWD_LOCK.lock().unwrap();

            std::env::remove_var("BDD_INFRA_TEST_REQUIRE_MISSING_BINARY");
            let old_target = std::env::var("CARGO_TARGET_DIR").ok();
            // Point at an empty dir so the target-dir branch misses too.
            let tmp = tempfile::tempdir().unwrap();
            std::env::set_var("CARGO_TARGET_DIR", tmp.path());

            let result =
                std::panic::catch_unwind(|| require_binary("bdd-infra-test-require-missing"));

            match old_target {
                Some(v) => std::env::set_var("CARGO_TARGET_DIR", v),
                None => std::env::remove_var("CARGO_TARGET_DIR"),
            }
            drop(lock);

            if let Err(payload) = result {
                std::panic::resume_unwind(payload);
            }
        }
    }

    // -----------------------------------------------------------------------
    // run_once tests
    //
    // These exercise doctor's one-shot subcommands (`tls issue`,
    // `auth hash-password`) plus rp's serve failure path. The binaries must
    // be pre-built — we either rely on the conventional env var being set
    // (Bazel path) or find them in the target dir for the Cargo path.
    // -----------------------------------------------------------------------

    /// Ensure `RP_BINARY` points at a pre-built `rp` binary, caching the
    /// result of [`find_binary`] in the env var so subsequent tests hit
    /// the env-var fast path instead of re-walking the cwd ancestors.
    ///
    /// Does NOT invoke `cargo build` — bdd-infra's contract is that
    /// binaries are pre-built by the caller (see the module-level docs).
    /// If rp is missing we panic with the same diagnostic the production
    /// path emits. Takes `CWD_LOCK` to serialize with sibling tests that
    /// mutate env vars [`find_binary`] reads.
    fn ensure_rp_binary() {
        let _lock = CWD_LOCK.lock().unwrap();
        if std::env::var_os("RP_BINARY").is_some() {
            return;
        }
        let binary = require_binary("rp");
        std::env::set_var("RP_BINARY", &binary);
    }

    /// [`ensure_rp_binary`]'s twin for the doctor binary, which owns the
    /// one-shot provisioning commands since doctor-plan D6a.
    fn ensure_doctor_binary() {
        let _lock = CWD_LOCK.lock().unwrap();
        if std::env::var_os("DOCTOR_BINARY").is_some() {
            return;
        }
        let binary = require_binary("doctor");
        std::env::set_var("DOCTOR_BINARY", &binary);
    }

    #[test]
    fn test_run_once_successful_command() {
        ensure_doctor_binary();
        let dir = tempfile::tempdir().unwrap();
        let output = run_once(
            "doctor",
            &["--config-dir", dir.path().to_str().unwrap(), "tls", "issue"],
            None,
        );
        assert!(
            output.status.success(),
            "tls issue should succeed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert!(
            dir.path().join("pki").join("ca.pem").exists(),
            "CA cert should exist under the config root's pki tree"
        );
    }

    #[test]
    fn test_run_once_captures_stderr_on_failure() {
        ensure_rp_binary();
        // An explicit --config naming a missing file is a hard error
        // (self-creation applies only to the XDG default path; a bare
        // `serve` would resolve XDG, scaffold a config, and serve forever).
        let output = run_once(
            "rp",
            &["serve", "--config", "/nonexistent/rp-test-config.json"],
            None,
        );
        assert!(!output.status.success());
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(!stderr.is_empty(), "stderr should contain error message");
    }

    #[test]
    fn test_run_once_with_stdin() {
        ensure_doctor_binary();
        let output = run_once(
            "doctor",
            &["auth", "hash-password", "--stdin"],
            Some(b"test-password\n"),
        );

        assert!(
            output.status.success(),
            "hash-password should succeed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        let hash = String::from_utf8(output.stdout).unwrap();
        assert!(
            hash.trim().starts_with("$argon2id$"),
            "expected Argon2id hash, got: {hash}"
        );
    }

    // -----------------------------------------------------------------------
    // send_sigterm tests
    // -----------------------------------------------------------------------

    /// Signal-send tests, in their own module so a Bazel target can select
    /// them by name: the enclosing `tests::` module is tagged
    /// `requires-cargo` for the three `run_once_*` tests that shell out for
    /// pre-built binaries, which keeps it out of every per-PR run. These are
    /// hermetic — one `kill(2)` at a pid that cannot exist, no subprocess,
    /// no filesystem.
    #[cfg(unix)]
    mod signals {
        use super::*;

        /// A send to a pid that cannot exist takes the failure arm, so the
        /// errno read and its log run. The signal itself goes nowhere: POSIX
        /// gives special meaning only to pids `0`, `-1` and negatives, so a
        /// positive pid addresses exactly one process, and no system
        /// allocates one anywhere near `i32::MAX`.
        ///
        /// Nothing observable comes back — the log is an unasserted side
        /// effect per testing.md 6.8 — so the probe below is what makes this
        /// a test of the failure arm rather than of whichever arm happened
        /// to run.
        #[test]
        fn test_send_sigterm_error_path_for_a_pid_that_cannot_exist() {
            let pid = i32::MAX.cast_unsigned();
            // SAFETY: libc::kill with signal 0 runs the kernel's error
            // checks without sending anything, which is exactly the
            // precondition to confirm.
            let probe = unsafe { libc::kill(pid.cast_signed(), 0) };
            assert_eq!(probe, -1, "the pid under test must not exist");

            send_sigterm(pid);
        }
    }
}
