//! A service's restart on request
//! (docs/crates/rusty-photon-service-lifecycle.md § Restart on request).
//!
//! A request cancels the shutdown token, so the service runs the stop path
//! it already has, and the runner exits with [`RESTART_EXIT_CODE`] once the
//! run closure returns. A watchdog thread exits with the same code when the
//! close has not finished within the deadline.

use std::fmt;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use tokio_util::sync::CancellationToken;

/// The code a service exits with when it restarts on request.
///
/// It is sysexits' `EX_TEMPFAIL`. Every service manager the packaging
/// installs restarts a service that exits with a code other than 0, and 1
/// is the code of a failed run, so a journal or the SCM's event log tells a
/// requested restart from a failure.
pub const RESTART_EXIT_CODE: i32 = 75;

/// How long a service asked to restart has to close its devices before it
/// exits anyway: a wedged camera can hang its close.
pub const RESTART_CLOSE_DEADLINE: Duration = Duration::from_secs(10);

/// The restart's exit: `std::process::exit` in console mode, a
/// `SERVICE_STOPPED` report and then the exit under the SCM.
pub type ExitFn = Arc<dyn Fn() + Send + Sync>;

/// Set by the first thread to exit for a restart, so a watchdog that fires
/// while the runner is already exiting waits instead of exiting a second
/// time.
static EXITING: AtomicBool = AtomicBool::new(false);

/// Claim the process's exit, or, when another thread has claimed it
/// already, wait for that thread to end the process.
pub fn claim_exit_or_wait() {
    if EXITING.swap(true, Ordering::SeqCst) {
        loop {
            std::thread::park();
        }
    }
}

/// Exit with `code`, unless another thread is exiting already.
pub fn exit_once(code: i32) -> ! {
    claim_exit_or_wait();
    std::process::exit(code)
}

/// A handle that asks the service to restart.
///
/// Handed out by [`crate::Shutdown::restart`]; clone it into whatever takes
/// the request (a driver service's HTTP route). Every clone shares one
/// request.
#[derive(Clone)]
pub struct Restart {
    inner: Arc<Inner>,
}

struct Inner {
    token: CancellationToken,
    requested: AtomicBool,
    /// The deadline and the exit the watchdog runs when it passes; `None`
    /// for a restart made outside the runner ([`Restart::new`]).
    watchdog: Option<(Duration, ExitFn)>,
}

impl fmt::Debug for Restart {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Restart")
            .field("requested", &self.is_requested())
            .field(
                "deadline",
                &self.inner.watchdog.as_ref().map(|(deadline, _)| deadline),
            )
            .finish_non_exhaustive()
    }
}

impl Restart {
    /// A restart whose request cancels `token` and does nothing else: no
    /// deadline, and no exit, since no runner is there to turn the stop
    /// into one.
    ///
    /// The runner makes the restart a service uses; this exists so a test
    /// can drive a service's run loop, or the routes that take the request,
    /// without ending its own process, as [`crate::ReloadSignal::new`] does
    /// for a reload.
    #[must_use]
    pub fn new(token: CancellationToken) -> Self {
        Self::build(token, None)
    }

    /// The runner's restart: its request also starts a watchdog that runs
    /// `exit` once `deadline` passes.
    pub(crate) fn with_exit(token: CancellationToken, deadline: Duration, exit: ExitFn) -> Self {
        Self::build(token, Some((deadline, exit)))
    }

    fn build(token: CancellationToken, watchdog: Option<(Duration, ExitFn)>) -> Self {
        Self {
            inner: Arc::new(Inner {
                token,
                requested: AtomicBool::new(false),
                watchdog,
            }),
        }
    }

    /// Ask the service to restart: stop it as a shutdown does, then exit
    /// with [`RESTART_EXIT_CODE`], within [`RESTART_CLOSE_DEADLINE`] even
    /// when the close hangs. A second request does nothing.
    pub fn request(&self) {
        if self.inner.requested.swap(true, Ordering::SeqCst) {
            return;
        }
        tracing::info!(
            "restart requested: stopping, then exiting with code {RESTART_EXIT_CODE} for the \
             service manager to start the service again"
        );
        if let Some((deadline, exit)) = &self.inner.watchdog {
            start_watchdog(*deadline, Arc::clone(exit));
        }
        self.inner.token.cancel();
    }

    /// Whether a restart has been requested.
    #[must_use]
    pub fn is_requested(&self) -> bool {
        self.inner.requested.load(Ordering::SeqCst)
    }
}

/// Run `exit` on a thread of its own once `deadline` passes. A thread
/// rather than a task, so a close that blocks a runtime worker, or the
/// thread polling the run closure, cannot hold it.
fn start_watchdog(deadline: Duration, exit: ExitFn) {
    let watchdog = std::thread::Builder::new()
        .name("restart-deadline".to_string())
        .spawn(move || {
            std::thread::sleep(deadline);
            tracing::warn!(
                "the service did not close within {deadline:?} of the restart request; exiting \
                 with code {RESTART_EXIT_CODE} without it"
            );
            exit();
        });
    if let Err(e) = watchdog {
        tracing::warn!(
            error = %e,
            "could not start the restart deadline's thread; the restart waits for the close"
        );
    }
}

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
mod tests {
    use std::sync::atomic::AtomicUsize;
    use std::sync::mpsc;

    use super::*;

    fn counting(deadline: Duration) -> (Restart, CancellationToken, Arc<AtomicUsize>) {
        let token = CancellationToken::new();
        let exits = Arc::new(AtomicUsize::new(0));
        let counter = Arc::clone(&exits);
        let restart = Restart::with_exit(
            token.clone(),
            deadline,
            Arc::new(move || {
                counter.fetch_add(1, Ordering::SeqCst);
            }),
        );
        (restart, token, exits)
    }

    #[test]
    fn a_request_cancels_the_shutdown_token_and_marks_the_restart() {
        let (restart, token, _) = counting(Duration::from_secs(3600));
        assert!(!restart.is_requested());
        restart.request();
        assert!(token.is_cancelled());
        assert!(restart.is_requested());
    }

    #[test]
    fn the_deadline_runs_the_exit_once_it_passes() {
        let token = CancellationToken::new();
        let (tx, rx) = mpsc::channel();
        let restart = Restart::with_exit(
            token,
            Duration::from_millis(20),
            Arc::new(move || {
                let _ = tx.send(());
            }),
        );
        restart.request();
        rx.recv_timeout(Duration::from_secs(10))
            .expect("the watchdog must run the exit once the deadline passes");
    }

    #[test]
    fn a_second_request_starts_no_second_watchdog() {
        let (restart, _, exits) = counting(Duration::from_millis(20));
        restart.request();
        restart.request();
        std::thread::sleep(Duration::from_millis(500));
        assert_eq!(exits.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn a_restart_made_outside_the_runner_only_cancels_its_token() {
        let token = CancellationToken::new();
        let restart = Restart::new(token.clone());
        restart.request();
        assert!(token.is_cancelled());
        assert!(restart.is_requested());
        assert!(format!("{restart:?}").contains("deadline: None"));
    }

    #[test]
    fn no_request_starts_no_watchdog() {
        let (restart, token, exits) = counting(Duration::from_millis(1));
        std::thread::sleep(Duration::from_millis(100));
        assert_eq!(exits.load(Ordering::SeqCst), 0);
        assert!(!token.is_cancelled());
        assert!(format!("{restart:?}").contains("requested: false"));
    }
}
