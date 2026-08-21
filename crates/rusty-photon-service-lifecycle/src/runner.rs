use std::future::Future;

use crate::{ReloadSignal, Shutdown};

/// Result type every service binary's `main` returns.
///
/// The error side is [`color_eyre::Report`], so a startup failure or fatal
/// exit escaping `main` prints a readable multi-line report with the full
/// `source()` chain instead of a single-line `Debug` dump.
///
/// Per ADR-011, this crate is the *only* place `color-eyre` enters the
/// workspace: services name this alias (and [`Report`](color_eyre::Report))
/// but never construct ad-hoc `eyre!` errors — errors stay `thiserror`-typed
/// everywhere below the binary boundary.
pub type ServiceResult = Result<(), color_eyre::Report>;

/// Boxed error the closures passed to [`ServiceRunner::run`] /
/// [`ServiceRunner::run_with_reload`] return.
///
/// Any typed `thiserror` error converts into it via `?`, as do plain string
/// errors (`"...".into()`). The runner converts it into the
/// [`color_eyre::Report`] that `main` returns, preserving the full `source()`
/// chain (see [`report_from_boxed`]). `Send + Sync` is required for that
/// conversion — `Report` only wraps thread-safe errors.
pub type RunError = Box<dyn std::error::Error + Send + Sync + 'static>;

/// Result type the run closures return. Converted to [`ServiceResult`] at
/// the runner's boundary.
pub type RunResult = Result<(), RunError>;

/// Adapter that gives an already-boxed [`RunError`] the Sized
/// `std::error::Error` impl [`color_eyre::Report::new`] needs, delegating
/// `Display`/`Debug`/`source()` to the inner error so the report renders the
/// original message and chain unchanged.
struct BoxedRunError(RunError);

impl std::fmt::Display for BoxedRunError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        std::fmt::Display::fmt(&self.0, f)
    }
}

impl std::fmt::Debug for BoxedRunError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        std::fmt::Debug::fmt(&self.0, f)
    }
}

impl std::error::Error for BoxedRunError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        self.0.source()
    }
}

/// Convert a boxed [`RunError`] into the [`color_eyre::Report`] that `main`
/// returns, preserving the `source()` chain.
///
/// The runner applies this at its own boundary; `?` cannot do it implicitly
/// because `Report` has no `From` impl for boxed trait objects. Public for
/// the rare fallible step that must run *before* [`ServiceRunner::run`]
/// (config-path resolution, identity minting) when the helper returns a
/// boxed error. It only converts an error that already exists — it is not a
/// substitute for `eyre!`-style ad-hoc error construction, which stays out
/// of service code per ADR-011.
///
/// `#[track_caller]` so the report's `Location:` section names the call
/// site (the service's `main`, or the runner boundary) rather than this
/// function.
#[track_caller]
pub fn report_from_boxed(e: RunError) -> color_eyre::Report {
    color_eyre::Report::new(BoxedRunError(e))
}

/// Install the `color-eyre` error/panic hooks exactly once per process.
///
/// `color_eyre::install()` is process-global and errors on a second call;
/// the `Once` guard makes repeated [`ServiceRunner`] invocations (the crate's
/// own tests run many per process) safe. An install failure is logged rather
/// than propagated — the service must still start even if another component
/// already claimed the hooks.
///
/// Called from both [`init_tracing`](crate::init_tracing) (so failures and
/// panics *before* the runner — config load, identity minting — render the
/// same formatted report) and the runner (so a service skipping
/// `init_tracing` still gets the hooks).
pub fn install_error_reporting() {
    static INSTALL: std::sync::Once = std::sync::Once::new();
    INSTALL.call_once(|| {
        if let Err(e) = color_eyre::install() {
            tracing::warn!("failed to install color-eyre error/panic hooks: {e}");
        }
    });
}

/// Process-global "running as a Windows service" flag. Set (never cleared)
/// when SCM mode engages: by [`init_service_tracing`](crate::init_service_tracing)
/// as soon as it sees `scm_mode = true`, and again — belt and braces — by the
/// runner's SCM dispatch path.
static SCM_SERVICE_MODE: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// True when this process is running as a Windows service (SCM mode).
///
/// Under SCM both std handles are absent: raw `println!`/`eprintln!` output
/// is silently lost (dead-handle writes sink in the common case, and a
/// genuinely invalid handle would error), so diagnostics belong in `tracing`
/// — which [`init_service_tracing`](crate::init_service_tracing) points at
/// the rolling log file in SCM mode. Use this accessor to gate the raw
/// std-handle writes that remain on the service path, most notably the
/// `bound_addr=` stdout handshake `bdd-infra`'s port parser reads. The BDD
/// harness never passes `--service`, so gating never breaks port discovery:
///
/// ```no_run
/// # let local_addr = "127.0.0.1:0";
/// // stdout handshake for bdd-infra's port parser (console mode only).
/// if !rusty_photon_service_lifecycle::is_scm_service() {
///     println!("Bound Alpaca server bound_addr={local_addr}");
/// }
/// ```
///
/// Always `false` on non-Windows targets and in console mode.
pub fn is_scm_service() -> bool {
    SCM_SERVICE_MODE.load(std::sync::atomic::Ordering::Relaxed)
}

/// Mark this process as running under the Windows SCM. Called from the SCM
/// branches of [`init_service_tracing`](crate::init_service_tracing) and the
/// runner's dispatch; never cleared — service mode is a process-lifetime
/// property. Compiled on every target so the flag contract stays
/// unit-testable cross-platform.
#[cfg_attr(not(all(windows, feature = "scm")), allow(dead_code))]
pub fn set_scm_service() {
    SCM_SERVICE_MODE.store(true, std::sync::atomic::Ordering::Relaxed);
}

/// Builder for a Rusty Photon service binary's lifecycle.
///
/// Owns the tokio runtime, installs OS signal handlers (or dispatches to the
/// Windows Service Control Manager when `scm` feature + [`Self::scm_mode`]
/// are enabled), and invokes the user closure with a [`Shutdown`] handle.
///
/// ## Usage
///
/// ```no_run
/// use rusty_photon_service_lifecycle::{ServiceResult, ServiceRunner};
///
/// fn main() -> ServiceResult {
///     ServiceRunner::new("my-service").run(|shutdown| async move {
///         // build server, race against shutdown.cancelled()
///         let _ = shutdown;
///         Ok(())
///     })
/// }
/// ```
///
/// For a service that also needs reload (filemonitor-style), enable
/// [`Self::with_reload`] and call [`Self::run_with_reload`]:
///
/// ```no_run
/// use rusty_photon_service_lifecycle::{ServiceResult, ServiceRunner};
///
/// fn main() -> ServiceResult {
///     ServiceRunner::new("my-service")
///         .with_reload()
///         .run_with_reload(|shutdown, reload| async move {
///             let _ = (shutdown, reload);
///             Ok(())
///         })
/// }
/// ```
pub struct ServiceRunner {
    name: &'static str,
    reload: bool,
    scm_mode: bool,
}

impl ServiceRunner {
    /// Create a runner with the given service name. The name is used for
    /// SCM registration (when `scm_mode` is on) and is otherwise informational.
    #[must_use]
    pub const fn new(name: &'static str) -> Self {
        Self {
            name,
            reload: false,
            scm_mode: false,
        }
    }

    /// Enable the reload signal. Required before [`Self::run_with_reload`].
    ///
    /// When enabled, the runner additionally installs `SIGHUP` handling
    /// (Unix) or accepts `ServiceControl::ParamChange` (Windows SCM mode).
    /// Each event wakes the [`ReloadSignal`] passed to the user closure.
    #[must_use]
    pub const fn with_reload(mut self) -> Self {
        self.reload = true;
        self
    }

    /// Windows SCM dispatch toggle. When `enable` is `true` *and* the
    /// `scm` cargo feature is on *and* the target is Windows, the runner
    /// registers with the Windows Service Control Manager (translating
    /// `Stop` and `ParamChange` events into shutdown/reload). Otherwise
    /// (non-Windows, feature off, or `enable = false`), runs in console
    /// mode with OS signal handlers.
    ///
    /// The method itself is always available across features and platforms
    /// — call sites do not need `cfg` gates. Service binaries typically
    /// wire `enable` to a hidden CLI flag passed by SCM (`--service`).
    #[must_use]
    pub const fn scm_mode(mut self, enable: bool) -> Self {
        self.scm_mode = enable;
        self
    }

    /// Build a multi-thread tokio runtime, install signal handlers (or
    /// dispatch SCM), and invoke `run_fn` with a [`Shutdown`] handle.
    /// Blocks until `run_fn`'s future resolves.
    ///
    /// Also installs the process-global `color-eyre` error/panic hooks
    /// (once per process), so every service gets formatted panic reports —
    /// with span context when [`init_tracing`](crate::init_tracing) is in
    /// use — without any per-service wiring.
    ///
    /// Returns the error from `run_fn`, if any. Signal-install failures are
    /// logged via `tracing::warn!` rather than returned.
    ///
    /// # Errors
    ///
    /// Returns whatever error `run_fn` resolved to; the runner adds no
    /// failure modes of its own.
    pub fn run<F, Fut>(self, run_fn: F) -> ServiceResult
    where
        F: FnOnce(Shutdown) -> Fut + Send + 'static,
        Fut: Future<Output = RunResult> + 'static,
    {
        install_error_reporting();

        #[cfg(all(windows, feature = "scm"))]
        if self.scm_mode {
            return scm::dispatch(
                self.name,
                scm::BoxedRunFn::Plain(Box::new(move |s| Box::pin(run_fn(s)))),
            );
        }

        run_console_plain(self.name, run_fn)
    }

    /// Like [`Self::run`] but also passes a [`ReloadSignal`]. Requires
    /// [`Self::with_reload`] to have been set on the builder.
    ///
    /// # Errors
    ///
    /// Returns an error immediately if the builder lacks
    /// [`Self::with_reload`]; otherwise whatever error `run_fn` resolved
    /// to.
    pub fn run_with_reload<F, Fut>(self, run_fn: F) -> ServiceResult
    where
        F: FnOnce(Shutdown, ReloadSignal) -> Fut + Send + 'static,
        Fut: Future<Output = RunResult> + 'static,
    {
        install_error_reporting();

        if !self.reload {
            return Err(color_eyre::eyre::eyre!(
                "ServiceRunner::run_with_reload requires .with_reload() on the builder"
            ));
        }

        #[cfg(all(windows, feature = "scm"))]
        if self.scm_mode {
            return scm::dispatch(
                self.name,
                scm::BoxedRunFn::WithReload(Box::new(move |s, r| Box::pin(run_fn(s, r)))),
            );
        }

        run_console_with_reload(self.name, run_fn)
    }
}

fn build_runtime() -> Result<tokio::runtime::Runtime, std::io::Error> {
    tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
}

fn run_console_plain<F, Fut>(name: &'static str, run_fn: F) -> ServiceResult
where
    F: FnOnce(Shutdown) -> Fut + Send + 'static,
    Fut: Future<Output = RunResult>,
{
    let rt = build_runtime()?;
    let token = tokio_util::sync::CancellationToken::new();
    let installed = spawn_shutdown_watcher(&rt, name, token.clone());
    rt.block_on(async move {
        let _ = installed.await;
        run_fn(Shutdown::from_token(token)).await
    })
    .map_err(report_from_boxed)
}

fn run_console_with_reload<F, Fut>(name: &'static str, run_fn: F) -> ServiceResult
where
    F: FnOnce(Shutdown, ReloadSignal) -> Fut + Send + 'static,
    Fut: Future<Output = RunResult>,
{
    let rt = build_runtime()?;
    let token = tokio_util::sync::CancellationToken::new();
    let reload = ReloadSignal::new();
    let installed = spawn_shutdown_watcher(&rt, name, token.clone());
    #[cfg(unix)]
    rt.spawn(watch_reload_signal(reload.clone()));
    rt.block_on(async move {
        let _ = installed.await;
        run_fn(Shutdown::from_token(token), reload).await
    })
    .map_err(report_from_boxed)
}

/// The platform's "stop this process" event, for the shutdown log line.
#[cfg(unix)]
const TERMINATE_EVENT: &str = "SIGTERM";
#[cfg(windows)]
const TERMINATE_EVENT: &str = "Ctrl+Break";
#[cfg(not(any(unix, windows)))]
const TERMINATE_EVENT: &str = "a termination request";

/// Spawn the shutdown watcher and hand back a receiver that fires **once the
/// OS handlers are installed**.
///
/// Callers must await it before invoking the user closure. Installation is
/// not something a service may race: until the handlers exist, the platform
/// default disposition applies, and on both platforms that default is fatal —
/// SIGTERM terminates, and an unregistered Windows console control event
/// falls through to a handler that terminates. A service that reached its own
/// readiness handshake first would advertise itself as stoppable during a
/// window where being stopped kills it outright.
///
/// A failed install still releases the barrier: the service starts in the
/// degraded state the no-panic policy calls for, rather than hanging.
fn spawn_shutdown_watcher(
    rt: &tokio::runtime::Runtime,
    name: &'static str,
    token: tokio_util::sync::CancellationToken,
) -> tokio::sync::oneshot::Receiver<()> {
    let (installed_tx, installed_rx) = tokio::sync::oneshot::channel();
    rt.spawn(async move {
        // Installs synchronously, so the send below cannot outrun it.
        let signalled = install_shutdown_signals(name);
        let _ = installed_tx.send(());
        signalled.await;
        tracing::info!("{name}: shutdown signal received, terminating");
        token.cancel();
    });
    installed_rx
}

/// The one thing the watcher needs from a registered handler. Implemented per
/// platform because the handler types differ and share no upstream trait —
/// Windows alone has a distinct type per event.
#[cfg(any(unix, windows))]
trait SignalHandler {
    /// Resolve on this handler's next delivery.
    async fn delivered(&mut self);
}

#[cfg(unix)]
impl SignalHandler for tokio::signal::unix::Signal {
    async fn delivered(&mut self) {
        self.recv().await;
    }
}

#[cfg(windows)]
impl SignalHandler for tokio::signal::windows::CtrlC {
    async fn delivered(&mut self) {
        self.recv().await;
    }
}

#[cfg(windows)]
impl SignalHandler for tokio::signal::windows::CtrlBreak {
    async fn delivered(&mut self) {
        self.recv().await;
    }
}

/// Await a handler's next delivery, or never, if it failed to register.
///
/// A plain function rather than a macro so the `None` arm is real crate code:
/// a macro expands into its caller, and a test calling it would attribute the
/// arm to the test module — which coverage excludes, leaving the degraded
/// path both untested and invisible.
#[cfg(any(unix, windows))]
async fn next_signal<H: SignalHandler>(handler: &mut Option<H>) {
    match handler {
        Some(handler) => handler.delivered().await,
        None => std::future::pending().await,
    }
}

/// Register the handlers **synchronously**, returning the future that resolves
/// when one of them fires.
///
/// The two-step shape is what lets [`spawn_shutdown_watcher`] order
/// installation ahead of service startup: every constructor used here
/// registers on call, unlike `tokio::signal::ctrl_c()`, which registers only
/// when its future is first polled and so cannot be sequenced.
#[cfg(unix)]
fn install_shutdown_signals(name: &'static str) -> impl Future<Output = ()> {
    use tokio::signal::unix::{signal, SignalKind};

    let mut interrupt = install(name, "Ctrl+C", signal(SignalKind::interrupt()));
    let mut terminate = install(name, TERMINATE_EVENT, signal(SignalKind::terminate()));

    async move {
        tokio::select! {
            () = next_signal(&mut interrupt) => {
                tracing::debug!("{name}: received Ctrl+C, shutting down");
            }
            () = next_signal(&mut terminate) => {
                tracing::debug!("{name}: received {TERMINATE_EVENT}, shutting down");
            }
        }
    }
}

/// Windows console mode. Both events need their **own** registration:
/// CTRL_C_EVENT does not cover CTRL_BREAK_EVENT, which is what a supervisor
/// sends to stop a console-mode service by process group
/// (`GenerateConsoleCtrlEvent`, as bdd-infra's `ServiceHandle::stop` does).
/// An unregistered console control event falls through to the OS default
/// handler, which terminates the process outright — no cancellation, no
/// shutdown path, no flush, and no error anywhere to notice it by.
#[cfg(windows)]
fn install_shutdown_signals(name: &'static str) -> impl Future<Output = ()> {
    use tokio::signal::windows::{ctrl_break, ctrl_c};

    let mut interrupt = install(name, "Ctrl+C", ctrl_c());
    let mut terminate = install(name, TERMINATE_EVENT, ctrl_break());

    async move {
        tokio::select! {
            () = next_signal(&mut interrupt) => {
                tracing::debug!("{name}: received Ctrl+C, shutting down");
            }
            () = next_signal(&mut terminate) => {
                tracing::debug!("{name}: received {TERMINATE_EVENT}, shutting down");
            }
        }
    }
}

#[cfg(not(any(unix, windows)))]
fn install_shutdown_signals(_name: &'static str) -> impl Future<Output = ()> {
    std::future::pending::<()>()
}

/// Unwrap a handler registration, degrading to "this source never fires"
/// rather than panicking — a misconfigured environment that cannot install
/// one signal keeps the others.
#[cfg(any(unix, windows))]
fn install<T>(name: &'static str, event: &str, result: std::io::Result<T>) -> Option<T> {
    match result {
        Ok(handler) => Some(handler),
        Err(e) => {
            tracing::warn!("{name}: failed to install the {event} handler: {e}");
            None
        }
    }
}

#[cfg(unix)]
async fn watch_reload_signal(reload: ReloadSignal) {
    match tokio::signal::unix::signal(tokio::signal::unix::SignalKind::hangup()) {
        Ok(mut sig) => loop {
            sig.recv().await;
            tracing::debug!("received SIGHUP, requesting reload");
            reload.notify();
        },
        Err(e) => {
            tracing::warn!("failed to install SIGHUP handler: {e}");
            std::future::pending::<()>().await;
        }
    }
}

#[cfg(all(windows, feature = "scm"))]
mod scm {
    //! Windows Service Control Manager dispatch.
    //!
    //! Bridges the synchronous SCM entry point (`service_dispatcher::start`)
    //! to the tokio-based runner. The user closure is type-erased into a
    //! `Box<dyn FnOnce(...)>` and stashed in a `OnceLock` so the
    //! `extern "system" fn` SCM entry point can reach it.
    use super::*;
    use std::ffi::OsString;
    use std::pin::Pin;
    use std::sync::{Mutex, OnceLock};
    use windows_service::service::{
        ServiceControl, ServiceControlAccept, ServiceExitCode, ServiceState, ServiceStatus,
        ServiceType,
    };
    use windows_service::service_control_handler::{self, ServiceControlHandlerResult};

    type PlainFn = Box<dyn FnOnce(Shutdown) -> Pin<Box<dyn Future<Output = RunResult>>> + Send>;
    type WithReloadFn =
        Box<dyn FnOnce(Shutdown, ReloadSignal) -> Pin<Box<dyn Future<Output = RunResult>>> + Send>;

    pub(super) enum BoxedRunFn {
        Plain(PlainFn),
        WithReload(WithReloadFn),
    }

    struct ScmConfig {
        name: &'static str,
        run_fn: Mutex<Option<BoxedRunFn>>,
    }

    static SCM_CONFIG: OnceLock<ScmConfig> = OnceLock::new();

    /// The run closure's error, captured by the SCM service thread
    /// ([`run_service`]) so [`dispatch`] can return it from the main thread
    /// once `service_dispatcher::start` unblocks. Keeps `ServiceRunner::run`'s
    /// "returns the error from `run_fn`" contract identical in SCM and console
    /// modes (non-zero process exit code, `Report` rendered from `main`).
    static SCM_RUN_ERROR: Mutex<Option<RunError>> = Mutex::new(None);

    pub(super) fn dispatch(name: &'static str, run_fn: BoxedRunFn) -> ServiceResult {
        // Authoritative setter: SCM mode is engaging now. (init_service_tracing
        // normally set it already, from the same --service flag.)
        super::set_scm_service();

        SCM_CONFIG
            .set(ScmConfig {
                name,
                run_fn: Mutex::new(Some(run_fn)),
            })
            .map_err(|_| color_eyre::eyre::eyre!("ServiceRunner SCM config already initialised"))?;

        windows_service::service_dispatcher::start(name, ffi_service_main)?;

        // The service thread stores the closure's error rather than
        // returning it through the `extern "system"` boundary; surface it
        // here so SCM mode matches the console path's contract.
        if let Some(e) = SCM_RUN_ERROR
            .lock()
            .map_err(|_| color_eyre::eyre::eyre!("ServiceRunner SCM run-error mutex poisoned"))?
            .take()
        {
            let report = report_from_boxed(e);
            // Returning the Report renders it to stderr from `main` — a dead
            // handle under SCM. Emit the full rendered source() chain through
            // tracing too, so it lands in the rolling log file while the
            // service's TracingGuard is still held (it flushes on exit).
            tracing::error!("{name}: service run failed:\n{report:?}");
            return Err(report);
        }
        Ok(())
    }

    windows_service::define_windows_service!(ffi_service_main, service_main);

    fn service_main(_args: Vec<OsString>) {
        if let Err(e) = run_service() {
            tracing::error!("service-runner SCM dispatch failed: {e}");
        }
    }

    fn run_service() -> ServiceResult {
        let cfg = SCM_CONFIG.get().ok_or_else(|| {
            color_eyre::eyre::eyre!("ServiceRunner SCM config missing; dispatch() must run first")
        })?;

        let run_fn = cfg
            .run_fn
            .lock()
            .map_err(|_| color_eyre::eyre::eyre!("ServiceRunner SCM run_fn mutex poisoned"))?
            .take()
            .ok_or_else(|| {
                color_eyre::eyre::eyre!(
                    "ServiceRunner SCM run_fn already taken (re-entrant dispatch?)"
                )
            })?;

        let with_reload = matches!(run_fn, BoxedRunFn::WithReload(_));
        let token = tokio_util::sync::CancellationToken::new();
        let reload = ReloadSignal::new();

        let token_for_handler = token.clone();
        let reload_for_handler = reload.clone();

        let status_handle = service_control_handler::register(cfg.name, move |evt| match evt {
            ServiceControl::Stop => {
                token_for_handler.cancel();
                ServiceControlHandlerResult::NoError
            }
            ServiceControl::ParamChange if with_reload => {
                reload_for_handler.notify();
                ServiceControlHandlerResult::NoError
            }
            ServiceControl::Interrogate => ServiceControlHandlerResult::NoError,
            _ => ServiceControlHandlerResult::NotImplemented,
        })?;

        let controls_accepted = if with_reload {
            ServiceControlAccept::STOP | ServiceControlAccept::PARAM_CHANGE
        } else {
            ServiceControlAccept::STOP
        };

        status_handle.set_service_status(ServiceStatus {
            service_type: ServiceType::OWN_PROCESS,
            current_state: ServiceState::Running,
            controls_accepted,
            exit_code: ServiceExitCode::Win32(0),
            checkpoint: 0,
            wait_hint: std::time::Duration::default(),
            process_id: None,
        })?;

        let rt = tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()?;
        let shutdown = Shutdown::from_token(token);
        let result = match run_fn {
            BoxedRunFn::Plain(f) => rt.block_on(f(shutdown)),
            BoxedRunFn::WithReload(f) => rt.block_on(f(shutdown, reload)),
        };

        // Surface the closure's outcome to SCM. This is the failure-visibility
        // mechanism ADR-015 / windows-packaging W1 pins: on Err we still
        // report SERVICE_STOPPED, but with a non-zero exit code —
        // dwWin32ExitCode = ERROR_SERVICE_SPECIFIC_ERROR with
        // dwServiceSpecificExitCode = 1. The installer configures restart
        // failure actions *and* sets SERVICE_CONFIG_FAILURE_ACTIONS_FLAG
        // (failure actions on non-crash failures), so SCM counts a stop with
        // a non-zero exit code as a failure and runs the configured restart —
        // restoring the systemd `Restart=on-failure` contract the serial
        // drivers' eager-validation exits rely on. Reporting Win32(0) on
        // every stop would make failures look like clean shutdowns (no
        // restart, and ops tooling like services.msc shown a clean stop).
        let run_error = result.err();
        if let Some(e) = &run_error {
            // Under SCM the rolling log file is often the only place this
            // failure is visible besides the SCM stop record.
            tracing::error!("{}: service run failed: {e}", cfg.name);
        }
        let exit_code = if run_error.is_none() {
            ServiceExitCode::Win32(0)
        } else {
            ServiceExitCode::ServiceSpecific(1)
        };

        status_handle.set_service_status(ServiceStatus {
            service_type: ServiceType::OWN_PROCESS,
            current_state: ServiceState::Stopped,
            controls_accepted: ServiceControlAccept::empty(),
            exit_code,
            checkpoint: 0,
            wait_hint: std::time::Duration::default(),
            process_id: None,
        })?;

        // Stash the closure's error for dispatch() (on the main thread) to
        // return once the dispatcher unblocks, mirroring the console path.
        if let Some(e) = run_error {
            *SCM_RUN_ERROR.lock().map_err(|_| {
                color_eyre::eyre::eyre!("ServiceRunner SCM run-error mutex poisoned")
            })? = Some(e);
        }
        Ok(())
    }
}

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
#[allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::unreachable,
    unsafe_code
)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU32, Ordering};
    use std::sync::Arc;

    // Signal-install tests share global per-process signal state; serialize them
    // so concurrent runs do not steal each other's deliveries.
    static SIGNAL_TEST_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    #[test]
    fn scm_service_flag_defaults_false_and_set_flips_it_sticky() {
        // One test for both states: the flag is process-global and sticky
        // (never cleared), so the default-false and post-set assertions must
        // live in a single test — split across two, their outcome would
        // depend on test scheduling order. No other test may set the flag.
        assert!(
            !is_scm_service(),
            "SCM service flag must default to false (console mode)"
        );
        set_scm_service();
        assert!(
            is_scm_service(),
            "SCM service flag must read true once service mode engaged"
        );
    }

    #[test]
    fn run_invokes_closure_exactly_once_and_returns_ok() {
        let _guard = SIGNAL_TEST_LOCK.lock().unwrap();
        let calls = Arc::new(AtomicU32::new(0));
        let calls_for_closure = Arc::clone(&calls);

        let result = ServiceRunner::new("test-once").run(move |_shutdown| async move {
            calls_for_closure.fetch_add(1, Ordering::SeqCst);
            Ok(())
        });

        result.unwrap();
        assert_eq!(calls.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn run_propagates_closure_error() {
        let _guard = SIGNAL_TEST_LOCK.lock().unwrap();
        let result = ServiceRunner::new("test-err")
            .run(|_shutdown| async move { Err("closure failed".into()) });

        let err = result.unwrap_err();
        assert_eq!(err.to_string(), "closure failed");
    }

    #[test]
    fn repeated_runs_install_error_reporting_without_error() {
        // `color_eyre::install()` errors on a second call; the Once guard in
        // `install_error_reporting` must make back-to-back runs clean.
        let _guard = SIGNAL_TEST_LOCK.lock().unwrap();
        for _ in 0..2 {
            ServiceRunner::new("test-install-once")
                .run(|_shutdown| async move { Ok(()) })
                .unwrap();
        }
    }

    #[derive(Debug)]
    struct RootCause;

    impl std::fmt::Display for RootCause {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            write!(f, "port 11119 already in use")
        }
    }

    impl std::error::Error for RootCause {}

    #[derive(Debug)]
    struct StartupError(RootCause);

    impl std::fmt::Display for StartupError {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            write!(f, "server startup failed")
        }
    }

    impl std::error::Error for StartupError {
        fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
            Some(&self.0)
        }
    }

    #[test]
    fn run_error_renders_as_multi_line_report_with_source_chain() {
        // The whole point of returning `Report` from `main`: a typed error's
        // full `source()` chain must render over multiple lines, not as a
        // single `Debug` line.
        let _guard = SIGNAL_TEST_LOCK.lock().unwrap();
        let result = ServiceRunner::new("test-report")
            .run(|_shutdown| async move { Err(RunError::from(StartupError(RootCause))) });

        let rendered = format!("{:?}", result.unwrap_err());
        assert!(
            rendered.contains("server startup failed"),
            "report should contain the outer error, got:\n{rendered}"
        );
        assert!(
            rendered.contains("port 11119 already in use"),
            "report should contain the root cause, got:\n{rendered}"
        );
        assert!(
            rendered.lines().count() > 1,
            "report should span multiple lines, got:\n{rendered}"
        );
    }

    #[test]
    fn run_with_reload_requires_with_reload_flag() {
        let _guard = SIGNAL_TEST_LOCK.lock().unwrap();
        let result =
            ServiceRunner::new("test-reload-flag").run_with_reload(|_s, _r| async move { Ok(()) });

        let err = result.unwrap_err();
        assert!(
            err.to_string().contains("with_reload"),
            "error should mention with_reload, got: {err}"
        );
    }

    #[cfg(unix)]
    #[test]
    fn sigterm_cancels_shutdown_token() {
        let _guard = SIGNAL_TEST_LOCK.lock().unwrap();
        let observed_cancel = Arc::new(AtomicU32::new(0));
        let observed_for_closure = Arc::clone(&observed_cancel);

        let result = ServiceRunner::new("test-sigterm").run(move |shutdown| async move {
            // Schedule a self-SIGTERM after the closure starts awaiting.
            tokio::spawn(async {
                tokio::time::sleep(std::time::Duration::from_millis(50)).await;
                // Safety: raise() on the current process is the documented way to
                // self-signal; libc::raise is unsafe only because it touches global
                // process state.
                unsafe {
                    libc::raise(libc::SIGTERM);
                }
            });

            shutdown.cancelled().await;
            observed_for_closure.store(1, Ordering::SeqCst);
            Ok(())
        });

        result.unwrap();
        assert_eq!(observed_cancel.load(Ordering::SeqCst), 1);
    }

    /// The handlers must be installed *before* the user closure gets to run.
    ///
    /// Raising SIGTERM as the closure's very first act is survivable only if
    /// the handler is already in place; under the default disposition the
    /// process dies right here and takes the test binary with it. There is
    /// deliberately no sleep — any delay lets installation catch up and makes
    /// the test vacuous, which is exactly what
    /// `sigterm_cancels_shutdown_token`'s 50 ms head start does (it covers
    /// delivery, not ordering).
    ///
    /// Without the barrier this is a race the service loses silently in
    /// production too: a service that reaches its readiness handshake before
    /// the handlers exist advertises itself as stoppable during a window
    /// where being stopped kills it.
    #[cfg(unix)]
    #[test]
    fn handlers_install_before_the_closure_runs() {
        let _guard = SIGNAL_TEST_LOCK.lock().unwrap();
        let reached_shutdown = Arc::new(AtomicU32::new(0));
        let observed = Arc::clone(&reached_shutdown);

        let result = ServiceRunner::new("test-install-order").run(move |shutdown| async move {
            // Safety: raise() on the current process is the documented way to
            // self-signal; libc::raise is unsafe only because it touches
            // global process state.
            unsafe {
                libc::raise(libc::SIGTERM);
            }
            shutdown.cancelled().await;
            observed.store(1, Ordering::SeqCst);
            Ok(())
        });

        result.unwrap();
        assert_eq!(reached_shutdown.load(Ordering::SeqCst), 1);
    }

    /// Ctrl+C must still cancel after the move off `tokio::signal::ctrl_c()`.
    ///
    /// That wrapper *is* SIGINT on Unix, but it registers only when polled,
    /// which the install-before-start barrier cannot allow — so the runner now
    /// builds the same handler through `signal(SignalKind::interrupt())`.
    /// Nothing covered Ctrl+C before, which is a poor thing to assume about a
    /// mechanism one has just swapped out.
    #[cfg(unix)]
    #[test]
    fn sigint_cancels_shutdown_token() {
        let _guard = SIGNAL_TEST_LOCK.lock().unwrap();
        let observed_cancel = Arc::new(AtomicU32::new(0));
        let observed_for_closure = Arc::clone(&observed_cancel);

        let result = ServiceRunner::new("test-sigint").run(move |shutdown| async move {
            // Safety: raise() on the current process is the documented way to
            // self-signal; libc::raise is unsafe only because it touches
            // global process state.
            unsafe {
                libc::raise(libc::SIGINT);
            }
            shutdown.cancelled().await;
            observed_for_closure.store(1, Ordering::SeqCst);
            Ok(())
        });

        result.unwrap();
        assert_eq!(observed_cancel.load(Ordering::SeqCst), 1);
    }

    /// A registration that failed degrades to "this source never fires".
    ///
    /// Resolving instead of parking would be the dangerous failure: the select
    /// would complete immediately and shut the service down the moment it
    /// finished starting.
    #[cfg(unix)]
    #[tokio::test]
    async fn a_handler_that_failed_to_install_never_fires() {
        let mut missing: Option<tokio::signal::unix::Signal> = None;
        let waited = tokio::time::timeout(
            std::time::Duration::from_millis(50),
            next_signal(&mut missing),
        )
        .await;
        assert!(
            waited.is_err(),
            "an uninstalled handler resolved instead of parking — a service \
             would shut down as soon as it started"
        );
    }

    /// `install` keeps the no-panic contract: a failed registration is warned
    /// about and becomes `None`, leaving the other signals working.
    #[cfg(any(unix, windows))]
    #[test]
    fn install_degrades_a_failed_registration_to_none() {
        let refused = std::io::Error::new(std::io::ErrorKind::PermissionDenied, "handler refused");
        assert!(install::<i32>("test-install", "Ctrl+C", Err(refused)).is_none());
        assert_eq!(install("test-install", "Ctrl+C", Ok(7)), Some(7));
    }

    #[cfg(unix)]
    #[test]
    fn sighup_wakes_reload_signal() {
        let _guard = SIGNAL_TEST_LOCK.lock().unwrap();
        let woke = Arc::new(AtomicU32::new(0));
        let woke_for_closure = Arc::clone(&woke);

        let result = ServiceRunner::new("test-sighup")
            .with_reload()
            .run_with_reload(move |shutdown, reload| async move {
                // Self-raise SIGHUP shortly, then SIGTERM to shut down.
                tokio::spawn(async {
                    tokio::time::sleep(std::time::Duration::from_millis(50)).await;
                    unsafe {
                        libc::raise(libc::SIGHUP);
                    }
                    tokio::time::sleep(std::time::Duration::from_millis(50)).await;
                    unsafe {
                        libc::raise(libc::SIGTERM);
                    }
                });

                loop {
                    tokio::select! {
                        () = reload.recv() => {
                            woke_for_closure.fetch_add(1, Ordering::SeqCst);
                        }
                        () = shutdown.cancelled() => return Ok(()),
                    }
                }
            });

        result.unwrap();
        assert!(
            woke.load(Ordering::SeqCst) >= 1,
            "reload signal should have fired at least once"
        );
    }
}
