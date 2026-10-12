//! A camera driver's run loop under the re-scan: build a server, serve it
//! until something ends it, and build the next one.

use std::future::Future;
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use rusty_photon_service_lifecycle::ReloadSignal;
use tokio_util::sync::CancellationToken;
use tracing::debug;

use super::Ended;

/// The stop one server serves until: a shutdown, or a reload.
pub type Stop = Pin<Box<dyn Future<Output = ()> + Send + 'static>>;

/// Serve one server after another until a shutdown.
///
/// `serve` builds a server from a configuration and serves it until the
/// [`Stop`] it is handed resolves — on `shutdown` or on `reload` — or its
/// failed USB scan's re-scan ends it ([`Ended::Rescanned`]). After a reload
/// the next server is built from what `load` reads now. After a re-scan it is
/// built from the configuration the stopped server was serving: nobody asked
/// for that rebuild, so it applies no edit to the file that no reload has
/// applied. A shutdown ends the loop whatever else ended the server, so one
/// that lands with a reload, or while a server drains, never waits out a
/// rebuild. A reload that lands while a re-scanned server drains has no stop
/// future left to see it, so it is honoured before that rebuild, which then
/// builds from `load`.
///
/// # Errors
///
/// The first error `load` or `serve` returns, which ends the loop.
pub async fn serve_and_rebuild<C, E, Fut>(
    shutdown: CancellationToken,
    reload: ReloadSignal,
    mut load: impl FnMut() -> Result<C, E>,
    mut serve: impl FnMut(C, Stop) -> Fut,
) -> Result<(), E>
where
    C: Clone,
    Fut: Future<Output = Result<Ended, E>>,
{
    let mut serving = None;
    loop {
        let config = match serving.take() {
            Some(config) => config,
            None => load()?,
        };
        let reloaded = Arc::new(AtomicBool::new(false));
        let stop = {
            let reloaded = Arc::clone(&reloaded);
            let shutdown = shutdown.clone();
            let reload = reload.clone();
            async move {
                tokio::select! {
                    () = shutdown.cancelled() => {}
                    () = reload.recv() => reloaded.store(true, Ordering::SeqCst),
                }
            }
        };
        let ended = serve(config.clone(), Box::pin(stop)).await?;
        if shutdown.is_cancelled() {
            return Ok(());
        }
        if reloaded.load(Ordering::SeqCst) {
            debug!("reload signalled; rebuilding from the new configuration");
            continue;
        }
        if ended == Ended::Stopped {
            return Ok(());
        }
        if reload_pending(&reload).await {
            debug!("a reload landed while the re-scanned server drained; rebuilding from it");
            continue;
        }
        debug!("a failed USB scan succeeds again; rebuilding from the configuration it serves");
        serving = Some(config);
    }
}

/// Whether a reload was signalled with nobody waiting for it, taking it if
/// so: the signal keeps it for the next waiter, which a pending one meets at
/// once.
async fn reload_pending(reload: &ReloadSignal) -> bool {
    tokio::select! {
        biased;
        () = reload.recv() => true,
        () = std::future::ready(()) => false,
    }
}

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
mod tests {
    use super::*;
    use std::sync::Mutex;

    /// How a scripted server ends.
    #[derive(Clone, Copy)]
    enum Then {
        /// Signals a reload, then waits for its stop, as a server does.
        Reload,
        /// Its stop resolves on a reload, and a shutdown lands as it drains.
        ReloadWithShutdown,
        /// Its re-scan succeeds: reports `Rescanned` without its stop.
        Rescan,
        /// A reload lands as its re-scan succeeds.
        RescanWithReload,
        /// A shutdown lands as its re-scan succeeds.
        RescanWithShutdown,
    }

    /// Run the loop over servers scripted by `script`, with each `load`
    /// reading the next of 1, 2, 3…, and the configurations they served.
    /// The server after the script waits for a shutdown it signals itself.
    async fn run(script: &[Then]) -> Vec<u32> {
        let shutdown = CancellationToken::new();
        let reload = ReloadSignal::new();
        let served = Arc::new(Mutex::new(Vec::new()));
        let mut loads: u32 = 0;
        let mut servers = script.iter().copied();
        serve_and_rebuild(
            shutdown.clone(),
            reload.clone(),
            || -> Result<u32, ()> {
                loads = loads.saturating_add(1);
                Ok(loads)
            },
            |config, stop| {
                served.lock().unwrap().push(config);
                let then = servers.next();
                let (shutdown, reload) = (shutdown.clone(), reload.clone());
                async move {
                    match then {
                        None => shutdown.cancel(),
                        Some(Then::Reload) => reload.notify(),
                        Some(Then::ReloadWithShutdown) => {
                            reload.notify();
                            stop.await;
                            shutdown.cancel();
                            return Ok(Ended::Stopped);
                        }
                        Some(Then::Rescan) => return Ok(Ended::Rescanned),
                        Some(Then::RescanWithReload) => {
                            reload.notify();
                            return Ok(Ended::Rescanned);
                        }
                        Some(Then::RescanWithShutdown) => {
                            shutdown.cancel();
                            return Ok(Ended::Rescanned);
                        }
                    }
                    stop.await;
                    Ok(Ended::Stopped)
                }
            },
        )
        .await
        .unwrap();
        let served = served.lock().unwrap().clone();
        served
    }

    #[tokio::test]
    async fn a_shutdown_ends_the_loop_after_one_server() {
        assert_eq!(run(&[]).await, [1]);
    }

    #[tokio::test]
    async fn a_reload_rebuilds_from_what_the_file_says_now() {
        assert_eq!(run(&[Then::Reload]).await, [1, 2]);
    }

    #[tokio::test]
    async fn a_shutdown_that_lands_with_a_reload_ends_the_loop_without_a_rebuild() {
        assert_eq!(run(&[Then::ReloadWithShutdown]).await, [1]);
    }

    #[tokio::test]
    async fn a_rescan_rebuilds_from_the_configuration_it_served() {
        assert_eq!(run(&[Then::Rescan]).await, [1, 1]);
    }

    #[tokio::test]
    async fn a_reload_that_lands_as_a_rescan_succeeds_rebuilds_once_from_the_file() {
        assert_eq!(run(&[Then::RescanWithReload]).await, [1, 2]);
    }

    #[tokio::test]
    async fn a_shutdown_that_lands_as_a_rescan_succeeds_ends_the_loop() {
        assert_eq!(run(&[Then::RescanWithShutdown]).await, [1]);
    }

    #[tokio::test]
    async fn a_rescan_after_a_reload_keeps_the_reloaded_configuration() {
        assert_eq!(run(&[Then::Reload, Then::Rescan]).await, [1, 2, 2]);
    }

    #[tokio::test]
    async fn an_error_ends_the_loop() {
        let result = serve_and_rebuild(
            CancellationToken::new(),
            ReloadSignal::new(),
            || Ok(1),
            |_, _| async { Err::<Ended, _>("the bind failed") },
        )
        .await;
        assert_eq!(result, Err("the bind failed"));
    }

    #[test]
    fn a_reload_is_pending_only_once_signalled_and_only_once() {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .build()
            .unwrap();
        let reload = ReloadSignal::new();
        assert!(!runtime.block_on(reload_pending(&reload)));
        reload.notify();
        assert!(runtime.block_on(reload_pending(&reload)));
        assert!(!runtime.block_on(reload_pending(&reload)), "it was taken");
    }
}
