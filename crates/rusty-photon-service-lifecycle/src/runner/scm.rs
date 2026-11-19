//! Windows Service Control Manager dispatch.
//!
//! Bridges the synchronous SCM entry point (`service_dispatcher::start`)
//! to the tokio-based runner. The user closure is type-erased into a
//! `Box<dyn FnOnce(...)>` and stashed in a `OnceLock` so the
//! `extern "system" fn` SCM entry point can reach it.
use super::{
    build_runtime, report_from_boxed, Future, ReloadSignal, Restart, RunError, RunResult,
    ServiceResult, Shutdown, RESTART_CLOSE_DEADLINE, RESTART_EXIT_CODE,
};
use crate::restart::claim_exit_or_wait;
use std::ffi::OsString;
use std::pin::Pin;
use std::sync::{Arc, Mutex, OnceLock, PoisonError};
use std::time::Duration;
use tokio_util::sync::CancellationToken;
use windows_service::service::{
    ServiceControl, ServiceControlAccept, ServiceExitCode, ServiceState, ServiceStatus, ServiceType,
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

/// The run's error, captured by the SCM service thread
/// ([`report_stopped`]) so [`dispatch`] can return it from the main thread
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
    let stored = SCM_RUN_ERROR
        .lock()
        .map_err(|_| color_eyre::eyre::eyre!("ServiceRunner SCM run-error mutex poisoned"))?
        .take();
    if let Some(e) = stored {
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
            color_eyre::eyre::eyre!("ServiceRunner SCM run_fn already taken (re-entrant dispatch?)")
        })?;

    let with_reload = matches!(run_fn, BoxedRunFn::WithReload(_));
    let token = CancellationToken::new();
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

    serve(
        cfg.name,
        run_fn,
        token,
        reload,
        move |status| status_handle.set_service_status(status),
        build_runtime,
        &SCM_RUN_ERROR,
    )
}

/// Start the service, run the closure on it, and hand each state the
/// service passes through to `set_status`, which in a service reports it to
/// the SCM.
///
/// The runtime is built before `Running` is reported, so the SCM shows a
/// service Running only once it can run. A runtime that cannot be built, or
/// a `Running` report that fails, ends the start as a failed run: the
/// closure does not run, and the stop is reported as a failed run's is.
fn serve<S>(
    name: &'static str,
    run_fn: BoxedRunFn,
    token: CancellationToken,
    reload: ReloadSignal,
    set_status: S,
    build_runtime: impl FnOnce() -> std::io::Result<tokio::runtime::Runtime>,
    run_error: &Mutex<Option<RunError>>,
) -> ServiceResult
where
    S: Fn(ServiceStatus) -> windows_service::Result<()> + Clone + Send + Sync + 'static,
{
    let controls_accepted = match run_fn {
        BoxedRunFn::Plain(_) => ServiceControlAccept::STOP,
        BoxedRunFn::WithReload(_) => {
            ServiceControlAccept::STOP | ServiceControlAccept::PARAM_CHANGE
        }
    };
    let started = build_runtime().map_err(RunError::from).and_then(|rt| {
        set_status(service_status(
            ServiceState::Running,
            controls_accepted,
            ServiceExitCode::Win32(0),
        ))?;
        Ok(rt)
    });
    let rt = match started {
        Ok(rt) => rt,
        Err(e) => return report_stopped(name, Err(e), run_error, set_status),
    };

    // The deadline passed with the close still running: report the
    // restart's stop and end the process from the watchdog's thread.
    let restart_status = set_status.clone();
    let restart = Restart::with_exit(
        token.clone(),
        RESTART_CLOSE_DEADLINE,
        Arc::new(move || {
            claim_exit_or_wait();
            report_restart_stop(&restart_status, name);
            std::process::exit(RESTART_EXIT_CODE);
        }),
    );
    let shutdown = Shutdown::new(token, restart.clone());
    let result = match run_fn {
        BoxedRunFn::Plain(f) => rt.block_on(f(shutdown)),
        BoxedRunFn::WithReload(f) => rt.block_on(f(shutdown, reload)),
    };

    // A requested restart stops with the restart code, so the failure
    // actions start the service again and the stop record names the
    // cause. The runtime is shut down without waiting for its blocking
    // tasks, which a hung driver call may hold.
    if restart.is_requested() {
        claim_exit_or_wait();
        if let Err(e) = &result {
            tracing::warn!("{name}: the run ended with an error while restarting: {e}");
        }
        rt.shutdown_background();
        report_restart_stop(set_status, name);
        return Ok(());
    }

    report_stopped(name, result, run_error, set_status)
}

/// Report `SERVICE_STOPPED` with the run's outcome.
///
/// A failed run stops with a non-zero exit code —
/// `dwWin32ExitCode = ERROR_SERVICE_SPECIFIC_ERROR` with
/// `dwServiceSpecificExitCode = 1` — which the installer's failure actions
/// restart, as systemd's `Restart=on-failure` restarts the serial drivers'
/// eager-validation exits. Reporting `Win32(0)` on every stop would make
/// failures look like clean shutdowns: no restart, and a clean stop in
/// services.msc (docs/crates/rusty-photon-service-lifecycle.md § SCM mode).
///
/// The error is stashed for [`dispatch`] before the stop is reported, not
/// after: once the report is made the dispatcher can return on the main
/// thread and the process can end, and a report that fails must not lose
/// the error.
fn report_stopped(
    name: &str,
    result: RunResult,
    run_error: &Mutex<Option<RunError>>,
    set_status: impl FnOnce(ServiceStatus) -> windows_service::Result<()>,
) -> ServiceResult {
    let exit_code = match result {
        Ok(()) => ServiceExitCode::Win32(0),
        Err(e) => {
            // Under SCM the rolling log file is often the only place this
            // failure is visible besides the SCM stop record.
            tracing::error!("{name}: service run failed: {e}");
            // A poisoned lock must not skip the stop report below; the value
            // it guards is only ever replaced whole.
            *run_error.lock().unwrap_or_else(PoisonError::into_inner) = Some(e);
            ServiceExitCode::ServiceSpecific(1)
        }
    };
    set_status(service_status(
        ServiceState::Stopped,
        ServiceControlAccept::empty(),
        exit_code,
    ))?;
    Ok(())
}

/// Report `SERVICE_STOPPED` with the restart code as the service-specific
/// exit code.
fn report_restart_stop(
    set_status: impl FnOnce(ServiceStatus) -> windows_service::Result<()>,
    name: &str,
) {
    tracing::info!("{name}: stopping with code {RESTART_EXIT_CODE} to restart");
    let stopped = set_status(service_status(
        ServiceState::Stopped,
        ServiceControlAccept::empty(),
        ServiceExitCode::ServiceSpecific(RESTART_EXIT_CODE.unsigned_abs()),
    ));
    if let Err(e) = stopped {
        tracing::error!("{name}: could not report the restart's stop to the SCM: {e}");
    }
}

/// The status of a service in a process of its own, with no operation
/// pending.
const fn service_status(
    current_state: ServiceState,
    controls_accepted: ServiceControlAccept,
    exit_code: ServiceExitCode,
) -> ServiceStatus {
    ServiceStatus {
        service_type: ServiceType::OWN_PROCESS,
        current_state,
        controls_accepted,
        exit_code,
        checkpoint: 0,
        wait_hint: Duration::ZERO,
        process_id: None,
    }
}

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicBool, Ordering};

    const RUNNING: ServiceStatus = service_status(
        ServiceState::Running,
        ServiceControlAccept::STOP,
        ServiceExitCode::Win32(0),
    );
    const STOPPED_CLEAN: ServiceStatus = service_status(
        ServiceState::Stopped,
        ServiceControlAccept::empty(),
        ServiceExitCode::Win32(0),
    );
    const STOPPED_FAILED: ServiceStatus = service_status(
        ServiceState::Stopped,
        ServiceControlAccept::empty(),
        ServiceExitCode::ServiceSpecific(1),
    );

    /// What one `serve` reported, returned, and left for `dispatch`.
    struct Served {
        result: ServiceResult,
        reported: Vec<ServiceStatus>,
        run_error: Option<String>,
    }

    /// Serve `run_fn` with a status sink that records every status it is
    /// handed and refuses the report of the `refused` state, as the SCM
    /// refuses a report it cannot take.
    fn serve_once(
        run_fn: BoxedRunFn,
        refused: Option<ServiceState>,
        build_runtime: impl FnOnce() -> std::io::Result<tokio::runtime::Runtime>,
    ) -> Served {
        let reported = Arc::new(Mutex::new(Vec::new()));
        let sink = {
            let reported = Arc::clone(&reported);
            move |status: ServiceStatus| {
                let state = status.current_state;
                reported.lock().unwrap().push(status);
                if Some(state) == refused {
                    Err(windows_service::Error::Winapi(std::io::Error::other(
                        "refused",
                    )))
                } else {
                    Ok(())
                }
            }
        };
        let run_error = Mutex::new(None);
        let result = serve(
            "test-scm",
            run_fn,
            CancellationToken::new(),
            ReloadSignal::new(),
            sink,
            build_runtime,
            &run_error,
        );
        let reported = reported.lock().unwrap().clone();
        let run_error = run_error.into_inner().unwrap().map(|e| e.to_string());
        Served {
            result,
            reported,
            run_error,
        }
    }

    /// A closure that marks `ran` and ends with `outcome`.
    fn plain(ran: &Arc<AtomicBool>, outcome: fn() -> RunResult) -> BoxedRunFn {
        let ran = Arc::clone(ran);
        BoxedRunFn::Plain(Box::new(move |_shutdown| {
            Box::pin(async move {
                ran.store(true, Ordering::SeqCst);
                outcome()
            })
        }))
    }

    #[test]
    fn a_clean_run_reports_running_then_a_clean_stop() {
        let ran = Arc::new(AtomicBool::new(false));
        let served = serve_once(plain(&ran, || Ok(())), None, build_runtime);

        served.result.unwrap();
        assert_eq!(served.reported, [RUNNING, STOPPED_CLEAN]);
        assert_eq!(served.run_error, None);
        assert!(ran.load(Ordering::SeqCst), "the closure must run");
    }

    #[test]
    fn a_failed_run_stops_with_the_failure_code_and_leaves_its_error() {
        let ran = Arc::new(AtomicBool::new(false));
        let served = serve_once(
            plain(&ran, || Err("closure failed".into())),
            None,
            build_runtime,
        );

        served.result.unwrap();
        assert_eq!(served.reported, [RUNNING, STOPPED_FAILED]);
        assert_eq!(served.run_error.as_deref(), Some("closure failed"));
    }

    #[test]
    fn a_refused_stop_report_still_leaves_the_run_error() {
        let ran = Arc::new(AtomicBool::new(false));
        let served = serve_once(
            plain(&ran, || Err("closure failed".into())),
            Some(ServiceState::Stopped),
            build_runtime,
        );

        let refusal = served.result.unwrap_err();
        assert!(
            refusal.chain().any(|e| e.to_string() == "refused"),
            "serve must return the refused report: {refusal:?}"
        );
        assert_eq!(served.reported, [RUNNING, STOPPED_FAILED]);
        assert_eq!(served.run_error.as_deref(), Some("closure failed"));
    }

    #[test]
    fn a_runtime_that_cannot_be_built_stops_the_start_as_failed() {
        let ran = Arc::new(AtomicBool::new(false));
        let served = serve_once(plain(&ran, || Ok(())), None, || {
            Err(std::io::Error::other("no worker threads"))
        });

        served.result.unwrap();
        assert_eq!(served.reported, [STOPPED_FAILED]);
        assert_eq!(served.run_error.as_deref(), Some("no worker threads"));
        assert!(
            !ran.load(Ordering::SeqCst),
            "the closure must not run without a runtime"
        );
    }

    #[test]
    fn a_refused_running_report_stops_the_start_as_failed() {
        let ran = Arc::new(AtomicBool::new(false));
        let served = serve_once(
            plain(&ran, || Ok(())),
            Some(ServiceState::Running),
            build_runtime,
        );

        served.result.unwrap();
        assert_eq!(served.reported, [RUNNING, STOPPED_FAILED]);
        served
            .run_error
            .expect("the refused report must be the run's error");
        assert!(
            !ran.load(Ordering::SeqCst),
            "the closure must not run once the SCM refuses Running"
        );
    }

    #[test]
    fn a_run_with_reload_accepts_param_change() {
        let run_fn =
            BoxedRunFn::WithReload(Box::new(|_shutdown, _reload| Box::pin(async { Ok(()) })));
        let served = serve_once(run_fn, None, build_runtime);

        served.result.unwrap();
        assert_eq!(
            served
                .reported
                .first()
                .map(|status| status.controls_accepted),
            Some(ServiceControlAccept::STOP | ServiceControlAccept::PARAM_CHANGE)
        );
    }
}
