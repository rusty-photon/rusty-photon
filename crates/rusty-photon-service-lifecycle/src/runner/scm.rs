//! Windows Service Control Manager dispatch.
//!
//! Bridges the synchronous SCM entry point (`service_dispatcher::start`)
//! to the tokio-based runner. The user closure is type-erased into a
//! `Box<dyn FnOnce(...)>` and stashed in a `OnceLock` so the
//! `extern "system" fn` SCM entry point can reach it.
use super::{
    report_from_boxed, Future, ReloadSignal, Restart, RunError, RunResult, ServiceResult, Shutdown,
    RESTART_CLOSE_DEADLINE, RESTART_EXIT_CODE,
};
use crate::restart::claim_exit_or_wait;
use std::ffi::OsString;
use std::pin::Pin;
use std::sync::{Arc, Mutex, OnceLock};
use windows_service::service::{
    ServiceControl, ServiceControlAccept, ServiceExitCode, ServiceState, ServiceStatus, ServiceType,
};
use windows_service::service_control_handler::{
    self, ServiceControlHandlerResult, ServiceStatusHandle,
};

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
    let name = cfg.name;
    // The deadline passed with the close still running: report the
    // restart's stop and end the process from the watchdog's thread.
    let restart = Restart::with_exit(
        token.clone(),
        RESTART_CLOSE_DEADLINE,
        Arc::new(move || {
            claim_exit_or_wait();
            report_restart_stop(status_handle, name);
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
        report_restart_stop(status_handle, name);
        return Ok(());
    }

    // Surface the closure's outcome to SCM: on Err we still report
    // SERVICE_STOPPED, but with a non-zero exit code —
    // dwWin32ExitCode = ERROR_SERVICE_SPECIFIC_ERROR with
    // dwServiceSpecificExitCode = 1 — which the installer's failure
    // actions restart, as systemd's `Restart=on-failure` restarts the
    // serial drivers' eager-validation exits. Reporting Win32(0) on every
    // stop would make failures look like clean shutdowns: no restart, and
    // a clean stop in services.msc
    // (docs/crates/rusty-photon-service-lifecycle.md § SCM mode).
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
        *SCM_RUN_ERROR
            .lock()
            .map_err(|_| color_eyre::eyre::eyre!("ServiceRunner SCM run-error mutex poisoned"))? =
            Some(e);
    }
    Ok(())
}

/// Report `SERVICE_STOPPED` with the restart code as the service-specific
/// exit code.
fn report_restart_stop(status_handle: ServiceStatusHandle, name: &'static str) {
    tracing::info!("{name}: stopping with code {RESTART_EXIT_CODE} to restart");
    let stopped = status_handle.set_service_status(ServiceStatus {
        service_type: ServiceType::OWN_PROCESS,
        current_state: ServiceState::Stopped,
        controls_accepted: ServiceControlAccept::empty(),
        exit_code: ServiceExitCode::ServiceSpecific(RESTART_EXIT_CODE.unsigned_abs()),
        checkpoint: 0,
        wait_hint: std::time::Duration::default(),
        process_id: None,
    });
    if let Err(e) = stopped {
        tracing::error!("{name}: could not report the restart's stop to the SCM: {e}");
    }
}
