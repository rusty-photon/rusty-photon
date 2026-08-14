//! Tracing/logging initialization shared by every Rusty Photon service binary.
//!
//! All service binaries log to **stderr**, never stdout, for two standing
//! reasons:
//!
//! 1. Stdout is reserved for the machine-readable `bound_addr=<host>:<port>`
//!    line that `bdd-infra`'s `ServiceHandle` parses to discover a test-spawned
//!    service's port (and, more generally, for any structured handshake a
//!    supervisor might read).
//! 2. The BDD harness drains and *discards* a service's stdout, so anything
//!    logged there is invisible during tests. Stderr is the conventional place
//!    for diagnostics and is inherited by child processes by default, so logs
//!    flow to the same destination as the test binary's own output without extra
//!    wiring.
//!
//! Historically, logging to stdout also produced a flood of
//! `[tracing-subscriber] Unable to write an event ... Broken pipe` noise in CI:
//! the harness aborted its stdout-drain task before the child exited, closing
//! the pipe's read end while the child was still writing shutdown-path logs.
//! That harness defect has since been fixed (the drain stays open until the
//! child exits), so stderr logging is no longer *required* to avoid the EPIPE —
//! but it remains the right destination for reasons 1 and 2.
//!
//! Filtering follows `RUST_LOG` when set, otherwise falls back to the level the
//! binary passes in (typically its `--log-level` flag, defaulting to `info`).
//! This is the same `stderr` + `EnvFilter` pattern `rp` and `plate-solver` used
//! inline, hoisted here so all services share one implementation.
//!
//! **Windows SCM service mode is the one exception to "logs go to stderr".**
//! Under the Service Control Manager both std handles are absent — everything
//! written to stderr vanishes. [`init_service_tracing`] therefore swaps the
//! stderr writer for a `tracing-appender` rolling file under
//! `%PROGRAMDATA%\rusty-photon\logs\` (daily rotation, 14 files retained) when
//! a service runs with `--service` (ADR-015 / the windows-packaging plan, W1).
//! Console mode is byte-for-byte unchanged: `init_service_tracing` with
//! `scm_mode = false` — or on any non-Windows target, or without the `scm`
//! cargo feature — is exactly [`init_tracing`].
//!
//! The subscriber additionally carries a [`tracing_error::ErrorLayer`] so
//! `SpanTrace::capture()` sees the live span stack. This is what lets the
//! `color-eyre` panic hook installed by [`ServiceRunner`](crate::ServiceRunner)
//! include the active span context in panic reports (ADR-011); without it the
//! report would print "span trace disabled".

use tracing::Level;
use tracing_subscriber::filter::{EnvFilter, LevelFilter};
use tracing_subscriber::layer::SubscriberExt;
use tracing_subscriber::util::SubscriberInitExt;

/// Build the [`EnvFilter`] for [`init_tracing`]: honor `RUST_LOG` when present
/// and parseable, otherwise fall back to `default_level`.
///
/// Split out from [`init_tracing`] so the filter logic is unit-testable without
/// installing a process-global subscriber. `RUST_LOG` takes precedence over
/// `default_level`, matching the convention `rp`/`plate-solver` established.
fn build_env_filter(default_level: Level) -> EnvFilter {
    EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| EnvFilter::new(LevelFilter::from_level(default_level).to_string()))
}

/// Build the subscriber [`init_tracing`] installs: stderr fmt output filtered
/// by [`build_env_filter`], plus a [`tracing_error::ErrorLayer`] so
/// `SpanTrace::capture()` (used by the `color-eyre` panic hook) observes the
/// live span stack.
///
/// Split out from [`init_tracing`] so the composition is unit-testable via
/// `tracing::subscriber::with_default` without touching the process-global
/// default subscriber.
fn build_subscriber(default_level: Level) -> impl tracing::Subscriber + Send + Sync {
    tracing_subscriber::registry()
        .with(build_env_filter(default_level))
        .with(tracing_subscriber::fmt::layer().with_writer(std::io::stderr))
        .with(tracing_error::ErrorLayer::default())
}

/// Initialize the global tracing subscriber for a service binary.
///
/// Logs are written to stderr (see the module docs for why), filtered by
/// `RUST_LOG` if set, otherwise at `default_level`. The subscriber carries a
/// [`tracing_error::ErrorLayer`] so panic reports include span context.
/// Idempotent: a redundant call (e.g. from a test that already installed a
/// subscriber) is ignored rather than panicking, matching
/// [`try_init`](tracing_subscriber::util::SubscriberInitExt::try_init)
/// semantics.
pub fn init_tracing(default_level: Level) {
    // Install the color-eyre hooks here as well as in the runner: services
    // call `init_tracing` before any fallible pre-runner work (config load,
    // identity minting), so errors and panics on that path render as
    // formatted reports too. Idempotent (`Once`-guarded).
    crate::runner::install_error_reporting();
    let _ = build_subscriber(default_level).try_init();
}

/// Keeps the SCM-mode rolling-file log writer flushing until process exit.
///
/// [`init_service_tracing`] returns one. Bind it in `main` for the whole
/// process lifetime — `let _tracing_guard = init_service_tracing(...);` —
/// so its `Drop` runs when `main` returns and the non-blocking writer's
/// buffered lines flush to disk (the final shutdown-path lines on an SCM
/// Stop would otherwise be lost with the background worker thread).
///
/// Do **not** bind it as a bare `_` (`let _ = ...` drops immediately). In
/// console mode — and on non-Windows targets or without the `scm` cargo
/// feature — the guard is inert and holding it costs nothing.
#[must_use = "bind as `let _tracing_guard = ...` and hold until process exit so buffered log lines flush on service stop"]
pub struct TracingGuard {
    #[cfg(feature = "scm")]
    _worker: Option<tracing_appender::non_blocking::WorkerGuard>,
}

impl TracingGuard {
    /// Guard for the console path: nothing to flush.
    const fn inert() -> Self {
        Self {
            #[cfg(feature = "scm")]
            _worker: None,
        }
    }
}

/// Initialize the global tracing subscriber for a service binary that can run
/// under the Windows Service Control Manager.
///
/// With `scm_mode = false` (console mode) this is exactly [`init_tracing`]:
/// logs go to stderr, filtered by `RUST_LOG` or `default_level`. With
/// `scm_mode = true` on Windows with the `scm` cargo feature, logs go to a
/// rolling file `%PROGRAMDATA%\rusty-photon\logs\<service_name>.<date>.log`
/// (daily rotation, 14 files retained; `ProgramData` env var, falling back to
/// `C:\ProgramData`) instead of the dead stderr handle SCM services get. If
/// the log file cannot be opened, falls back to the stderr subscriber rather
/// than failing service startup.
///
/// On non-Windows targets (or without the `scm` feature) `scm_mode` is a
/// no-op, matching [`ServiceRunner::scm_mode`](crate::ServiceRunner::scm_mode).
///
/// Idempotent like [`init_tracing`]. The returned [`TracingGuard`] must be
/// held until process exit — see its docs.
pub fn init_service_tracing(
    service_name: &str,
    default_level: Level,
    scm_mode: bool,
) -> TracingGuard {
    #[cfg(all(windows, feature = "scm"))]
    if scm_mode {
        // Mark the process as service-mode first, so is_scm_service() is
        // accurate for everything that runs after tracing init (e.g. the
        // gated `bound_addr=` stdout handshake).
        crate::runner::set_scm_service();
        return scm_file::init(service_name, default_level, &scm_file::default_log_dir());
    }

    #[cfg(not(all(windows, feature = "scm")))]
    let _ = (service_name, scm_mode);

    init_tracing(default_level);
    TracingGuard::inert()
}

/// SCM-mode rolling-file logging (windows-packaging plan, W1).
///
/// Only the Windows SCM branch of [`init_service_tracing`] calls into this
/// module in production, but it is compiled on every target (behind the `scm`
/// feature) so the path resolution and writer construction are unit-tested
/// cross-platform — hence the `allow(dead_code)` on non-Windows.
#[cfg(feature = "scm")]
#[cfg_attr(not(windows), allow(dead_code))]
mod scm_file {
    use super::{build_env_filter, init_tracing, Level, SubscriberExt, SubscriberInitExt};
    use std::ffi::OsString;
    use std::path::{Path, PathBuf};

    /// How many rotated daily log files to retain per service.
    const MAX_LOG_FILES: usize = 14;

    /// Resolve the Windows `ProgramData` directory from the given environment
    /// value: the `ProgramData` env var when set and non-empty, otherwise the
    /// stock `C:\ProgramData`.
    ///
    /// Private to this crate; `rusty-photon-config` grows its own resolver for
    /// the Windows config path (W2) — deduplication is a noted follow-up.
    fn program_data_dir(env_value: Option<OsString>) -> PathBuf {
        match env_value {
            Some(v) if !v.is_empty() => PathBuf::from(v),
            _ => PathBuf::from(r"C:\ProgramData"),
        }
    }

    /// The service log directory under a `ProgramData` root:
    /// `<program_data>\rusty-photon\logs`.
    fn log_dir_under(program_data: &Path) -> PathBuf {
        program_data.join("rusty-photon").join("logs")
    }

    /// The real SCM-mode log directory:
    /// `%PROGRAMDATA%\rusty-photon\logs` (per ADR-015).
    pub(super) fn default_log_dir() -> PathBuf {
        log_dir_under(&program_data_dir(std::env::var_os("ProgramData")))
    }

    /// Build the non-blocking rolling-file writer for a service: daily
    /// rotation, [`MAX_LOG_FILES`] retained, files named
    /// `<service_name>.<YYYY-MM-DD>.log` in `log_dir`. Creates `log_dir` if
    /// missing. The [`WorkerGuard`](tracing_appender::non_blocking::WorkerGuard)
    /// must outlive all logging (drop it to flush).
    fn build_rolling_writer(
        service_name: &str,
        log_dir: &Path,
    ) -> Result<
        (
            tracing_appender::non_blocking::NonBlocking,
            tracing_appender::non_blocking::WorkerGuard,
        ),
        tracing_appender::rolling::InitError,
    > {
        let appender = tracing_appender::rolling::Builder::new()
            .rotation(tracing_appender::rolling::Rotation::DAILY)
            .filename_prefix(service_name)
            .filename_suffix("log")
            .max_log_files(MAX_LOG_FILES)
            .build(log_dir)?;
        Ok(tracing_appender::non_blocking(appender))
    }

    /// Install the SCM-mode subscriber: same `EnvFilter` + `ErrorLayer`
    /// composition as [`init_tracing`], with the fmt layer writing to the
    /// rolling file (ANSI off — these are plain-text log files). Falls back
    /// to the stderr subscriber if the log file cannot be opened, so a
    /// mis-ACLed `ProgramData` never blocks service startup. Idempotent like
    /// [`init_tracing`]: a redundant call keeps the already-installed
    /// subscriber and returns an inert guard (see [`guard_for`]).
    pub(super) fn init(
        service_name: &str,
        default_level: Level,
        log_dir: &Path,
    ) -> super::TracingGuard {
        crate::runner::install_error_reporting();
        match build_rolling_writer(service_name, log_dir) {
            Ok((writer, worker)) => {
                let installed = tracing_subscriber::registry()
                    .with(build_env_filter(default_level))
                    .with(
                        tracing_subscriber::fmt::layer()
                            .with_writer(writer)
                            .with_ansi(false),
                    )
                    .with(tracing_error::ErrorLayer::default())
                    .try_init()
                    .is_ok();
                guard_for(worker, installed)
            }
            Err(e) => {
                // Nowhere good to report this: stderr is dead under SCM.
                // Fall back to the console subscriber (harmlessly sunk) so
                // the service still starts, and record why on the off chance
                // stderr is live (console run with --service, tests).
                init_tracing(default_level);
                tracing::warn!(
                    "failed to open rolling log file in {}: {e}; logging to stderr instead",
                    log_dir.display()
                );
                super::TracingGuard::inert()
            }
        }
    }

    /// Keep the non-blocking writer's worker only when the SCM subscriber
    /// actually became the global default. On a redundant call `try_init`
    /// fails (a global subscriber is already installed), so no events route
    /// to this writer — keeping the worker would leak one live background
    /// thread per call and hand the caller a guard unrelated to the active
    /// subscriber. Dropping the
    /// [`WorkerGuard`](tracing_appender::non_blocking::WorkerGuard) stops the
    /// worker, keeping [`init`] idempotent like [`init_tracing`].
    fn guard_for(
        worker: tracing_appender::non_blocking::WorkerGuard,
        installed: bool,
    ) -> super::TracingGuard {
        if installed {
            super::TracingGuard {
                _worker: Some(worker),
            }
        } else {
            drop(worker);
            super::TracingGuard::inert()
        }
    }

    #[cfg(test)]
    #[cfg_attr(coverage_nightly, coverage(off))]
    mod tests {
        use super::*;

        #[test]
        fn program_data_dir_uses_env_value_when_set() {
            let dir = program_data_dir(Some(OsString::from(r"D:\CustomData")));
            assert_eq!(dir, PathBuf::from(r"D:\CustomData"));
        }

        #[test]
        fn program_data_dir_falls_back_when_unset() {
            let dir = program_data_dir(None);
            assert_eq!(dir, PathBuf::from(r"C:\ProgramData"));
        }

        #[test]
        fn program_data_dir_falls_back_when_empty() {
            let dir = program_data_dir(Some(OsString::new()));
            assert_eq!(dir, PathBuf::from(r"C:\ProgramData"));
        }

        #[test]
        fn log_dir_is_rusty_photon_logs_under_program_data() {
            let dir = log_dir_under(Path::new(r"C:\ProgramData"));
            assert_eq!(
                dir,
                Path::new(r"C:\ProgramData")
                    .join("rusty-photon")
                    .join("logs")
            );
        }

        #[test]
        fn rolling_writer_creates_dated_service_log_file() {
            use std::io::Write as _;

            let tmp = tempfile::tempdir().unwrap();
            let log_dir = tmp.path().join("logs");

            let (mut writer, worker) = build_rolling_writer("test-svc", &log_dir).unwrap();

            writer
                .write_all(b"hello from the rolling writer\n")
                .unwrap();
            drop(writer);
            drop(worker); // flush

            let entries: Vec<_> = std::fs::read_dir(&log_dir)
                .unwrap()
                .map(|e| e.unwrap().file_name().into_string().unwrap())
                .collect();
            assert_eq!(entries.len(), 1, "expected one log file, got {entries:?}");
            let name = &entries[0];
            assert!(
                name.starts_with("test-svc.") && name.ends_with(".log"),
                "expected test-svc.<date>.log, got {name}"
            );
            let content = std::fs::read_to_string(log_dir.join(name)).unwrap();
            assert!(content.contains("hello from the rolling writer"));
        }

        #[test]
        fn rolling_writer_fails_when_log_dir_is_a_file() {
            let tmp = tempfile::tempdir().unwrap();
            let blocker = tmp.path().join("blocker");
            std::fs::write(&blocker, b"not a directory").unwrap();

            // `blocker` is a file, so `blocker/logs` cannot be created.
            let err = build_rolling_writer("test-svc", &blocker.join("logs")).unwrap_err();
            let _ = err.to_string(); // Display must not panic
        }

        #[test]
        fn init_creates_log_file() {
            let tmp = tempfile::tempdir().unwrap();
            let log_dir = tmp.path().join("logs");

            // Whether this call wins the global-subscriber install race
            // against sibling tests is nondeterministic (the guard state for
            // both outcomes is covered by the `guard_for` tests and the
            // deterministic second-call test below); the log file is created
            // eagerly by the writer either way.
            let _guard = init("init-svc", Level::INFO, &log_dir);

            let entries: Vec<_> = std::fs::read_dir(&log_dir)
                .unwrap()
                .map(|e| e.unwrap().file_name().into_string().unwrap())
                .collect();
            assert!(
                entries
                    .iter()
                    .any(|n| n.starts_with("init-svc.") && n.ends_with(".log")),
                "expected init-svc.<date>.log in {entries:?}"
            );
        }

        #[test]
        fn guard_for_keeps_worker_when_subscriber_installed() {
            let tmp = tempfile::tempdir().unwrap();
            let (_writer, worker) =
                build_rolling_writer("keep-svc", &tmp.path().join("logs")).unwrap();

            let guard = guard_for(worker, true);
            assert!(
                guard._worker.is_some(),
                "install succeeded: the worker guard must be kept for main to hold"
            );
        }

        #[test]
        fn guard_for_drops_worker_when_subscriber_not_installed() {
            let tmp = tempfile::tempdir().unwrap();
            let (_writer, worker) =
                build_rolling_writer("drop-svc", &tmp.path().join("logs")).unwrap();

            let guard = guard_for(worker, false);
            assert!(
                guard._worker.is_none(),
                "install failed: keeping the worker would leak its thread"
            );
        }

        #[test]
        fn init_when_global_subscriber_already_set_returns_inert_guard() {
            // Guarantee a global subscriber exists (init_tracing is
            // idempotent; whoever installed it first, one is set after this
            // line), so init's try_init deterministically fails here — the
            // redundant-call path must not keep a worker thread alive.
            init_tracing(Level::INFO);

            let tmp = tempfile::tempdir().unwrap();
            let guard = init("second-svc", Level::INFO, &tmp.path().join("logs"));
            assert!(
                guard._worker.is_none(),
                "a redundant init must return an inert guard, not a live worker"
            );
        }

        #[test]
        fn init_falls_back_to_inert_guard_when_log_dir_unusable() {
            let tmp = tempfile::tempdir().unwrap();
            let blocker = tmp.path().join("blocker");
            std::fs::write(&blocker, b"not a directory").unwrap();

            let guard = init("fallback-svc", Level::INFO, &blocker.join("logs"));
            assert!(
                guard._worker.is_none(),
                "expected the inert stderr fallback guard"
            );
        }
    }
}

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
mod tests {
    use super::*;

    // `RUST_LOG` is process-global; serialize the tests that read or write it so
    // they don't observe each other's mutations under `cargo test` (which runs
    // tests as threads in one process for the coverage job).
    static ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    #[test]
    fn falls_back_to_default_level_when_rust_log_unset() {
        let _lock = ENV_LOCK.lock().unwrap();
        let saved = std::env::var_os("RUST_LOG");
        std::env::remove_var("RUST_LOG");

        let hint = build_env_filter(Level::DEBUG).max_level_hint();

        if let Some(v) = saved {
            std::env::set_var("RUST_LOG", v);
        }
        assert_eq!(hint, Some(LevelFilter::DEBUG));
    }

    #[test]
    fn rust_log_overrides_default_level() {
        let _lock = ENV_LOCK.lock().unwrap();
        let saved = std::env::var_os("RUST_LOG");
        std::env::set_var("RUST_LOG", "trace");

        let hint = build_env_filter(Level::INFO).max_level_hint();

        match saved {
            Some(v) => std::env::set_var("RUST_LOG", v),
            None => std::env::remove_var("RUST_LOG"),
        }
        assert_eq!(hint, Some(LevelFilter::TRACE));
    }

    #[test]
    fn subscriber_captures_span_trace_via_error_layer() {
        let _lock = ENV_LOCK.lock().unwrap();
        let saved = std::env::var_os("RUST_LOG");
        std::env::remove_var("RUST_LOG");

        let subscriber = build_subscriber(Level::INFO);
        let status = tracing::subscriber::with_default(subscriber, || {
            let span = tracing::info_span!("capture_probe");
            let _entered = span.enter();
            tracing_error::SpanTrace::capture().status()
        });

        if let Some(v) = saved {
            std::env::set_var("RUST_LOG", v);
        }
        assert_eq!(status, tracing_error::SpanTraceStatus::CAPTURED);
    }

    #[test]
    fn init_tracing_is_idempotent() {
        // Two calls must not panic — the second is swallowed by `try_init`.
        init_tracing(Level::INFO);
        init_tracing(Level::INFO);
    }

    #[test]
    fn init_service_tracing_console_mode_returns_inert_guard() {
        // Console mode (scm_mode = false) is plain init_tracing: no file
        // writer is constructed on any platform, and repeated calls are
        // idempotent like init_tracing.
        let guard = init_service_tracing("console-svc", Level::INFO, false);
        #[cfg(feature = "scm")]
        assert!(
            guard._worker.is_none(),
            "console mode must not hold a rolling-file worker guard"
        );
        drop(guard);
    }
}
