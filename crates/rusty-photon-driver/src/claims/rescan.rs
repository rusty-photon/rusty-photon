//! The failed USB scan's background re-scan (the device-claims plan's D4.4).
//!
//! A driver whose USB scan failed serves a placeholder at every listed number
//! and opens nothing. A failed scan is never a startup failure, so nothing
//! else would retry it: the driver re-scans after waits of 10 s, 20 s and
//! 40 s, then every 60 s, and on the first try after which a rebuild would
//! serve the list it ends its own server ([`Ended::Rescanned`]). The driver's
//! run loop rebuilds it from the configuration that server was serving, not
//! from the config file: nobody asked for this rebuild, so it applies no edit
//! to the file that no reload has applied. That success ends that server only
//! — it is not the service-wide reload signal — so one landing after another
//! reload has already stopped the server reaches nothing, and cannot stop the
//! next one.
//!
//! What a try checks is the driver's ([`Rescan::new`]): its scan, then — only
//! once the scan runs — its SDK's enumeration. A rebuild whose enumeration
//! fails ends the service, so the re-scan waits that out too.

use std::future::Future;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use tokio::sync::oneshot;
use tracing::{debug, info, warn};

/// The waits between a failed scan's re-scans: 10 s, 20 s and 40 s, then
/// 60 s for as long as the scan keeps failing.
pub fn rescan_waits() -> impl Iterator<Item = Duration> {
    [10, 20, 40]
        .into_iter()
        .map(Duration::from_secs)
        .chain(std::iter::repeat(Duration::from_secs(60)))
}

/// What still stands between a re-scan and the rebuild it would ask for.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Blocker {
    /// The USB scan still fails.
    Scan(String),
    /// The SDK cannot enumerate, and a rebuild that cannot enumerate ends the
    /// service: with the first scan failed, the rebuild is the first
    /// enumeration since the start.
    Sdk(String),
}

impl Blocker {
    /// Whether this blocker is news to the operator after `previous`, the
    /// one the try before met: an SDK that blocks the rebuild is warned about
    /// each time it starts to, so one that clears and comes back is said
    /// again, while one that persists from the try before is not.
    fn is_new(&self, previous: Option<&Self>) -> bool {
        previous.is_none_or(|p| std::mem::discriminant(p) != std::mem::discriminant(self))
    }
}

/// What one try checks: what would keep a rebuild now from serving the list,
/// or `None` when nothing would. Blocking.
type Check = Arc<dyn Fn() -> Option<Blocker> + Send + Sync>;

/// What would keep a rebuild now from serving the list, or `None` when
/// nothing would: the scan's failure, else the SDK's. The SDK is asked only
/// once the scan runs. Blocking.
fn first_blocker(
    scan: impl FnOnce() -> Result<(), String>,
    enumerate: impl FnOnce() -> Result<(), String>,
) -> Option<Blocker> {
    if let Err(error) = scan() {
        return Some(Blocker::Scan(error));
    }
    enumerate().err().map(Blocker::Sdk)
}

/// The background re-scan of one server whose USB scan failed.
#[derive(Clone)]
pub struct Rescan {
    /// The SDK's display name, for the log.
    sdk: &'static str,
    check: Check,
}

impl std::fmt::Debug for Rescan {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Rescan")
            .field("sdk", &self.sdk)
            .finish_non_exhaustive()
    }
}

impl Rescan {
    /// A re-scan whose every try takes the driver's USB `scan` and, once that
    /// runs, has its `sdk` `enumerate`; each says why it failed. Both run on
    /// the blocking pool.
    pub fn new(
        sdk: &'static str,
        scan: impl Fn() -> Result<(), String> + Send + Sync + 'static,
        enumerate: impl Fn() -> Result<(), String> + Send + Sync + 'static,
    ) -> Self {
        Self::checking(sdk, move || first_blocker(&scan, &enumerate))
    }

    /// A re-scan whose every try runs `check`.
    fn checking(
        sdk: &'static str,
        check: impl Fn() -> Option<Blocker> + Send + Sync + 'static,
    ) -> Self {
        Self {
            sdk,
            check: Arc::new(check),
        }
    }

    /// One try, on the calling thread: what would keep a rebuild now from
    /// serving the list, or `None` when nothing would. Blocking.
    #[must_use]
    pub fn check(&self) -> Option<Blocker> {
        (self.check)()
    }

    /// Try after each of `waits` until a rebuild would serve the list, then
    /// say so on `ready`.
    async fn run(self, waits: impl Iterator<Item = Duration>, ready: oneshot::Sender<()>) {
        let mut previous = None;
        for wait in waits {
            tokio::time::sleep(wait).await;
            let check = Arc::clone(&self.check);
            let blocker = match tokio::task::spawn_blocking(move || check()).await {
                Ok(None) => {
                    info!("the USB scan succeeds again; rebuilding to place the listed cameras");
                    // A send to a server that has already stopped goes
                    // nowhere: the success belongs to this server alone.
                    let _ = ready.send(());
                    return;
                }
                Ok(Some(blocker)) => blocker,
                Err(error) => {
                    debug!(%error, "the USB re-scan task failed");
                    continue;
                }
            };
            let new = blocker.is_new(previous.as_ref());
            match &blocker {
                Blocker::Scan(error) => debug!(%error, "the USB scan still fails"),
                Blocker::Sdk(error) if new => {
                    warn!(
                        %error,
                        sdk = self.sdk,
                        "the USB scan succeeds again, but the SDK cannot enumerate; not \
                         rebuilding until it can"
                    );
                }
                Blocker::Sdk(error) => debug!(%error, "the rebuild is still blocked"),
            }
            previous = Some(blocker);
        }
    }
}

/// Why a server stopped serving.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Ended {
    /// The caller's stop future resolved.
    Stopped,
    /// A failed USB scan succeeds again: rebuild the server from the
    /// configuration it was serving, to place the listed cameras.
    Rescanned,
}

/// Which of a server's two stops ended it, read once it has stopped.
#[derive(Debug, Clone)]
pub struct StopCause(Arc<AtomicBool>);

impl StopCause {
    /// What ended the server.
    #[must_use]
    pub fn ended(&self) -> Ended {
        if self.0.load(Ordering::SeqCst) {
            Ended::Rescanned
        } else {
            Ended::Stopped
        }
    }
}

/// The stop a server with an optional re-scan serves until.
///
/// That is `shutdown`, or — when `rescan` is given — the re-scan finding that
/// a rebuild would serve the list, whichever comes first. The re-scan runs
/// while the server serves, and is aborted the moment the server starts to
/// stop, so it cannot leave a success for the next server; the returned
/// [`StopCause`] says which stop it was. A try already on the blocking pool
/// is not stopped by the abort: it runs to its end, which the scan's own
/// deadline bounds, and its answer goes nowhere. Its enumeration cannot
/// renumber the SDK's camera list under a rebuild's placement: `zwo-rs` and
/// `svbony-rs` serialise every enumeration on a process-wide lock.
///
/// Call it from within the runtime the server runs on: it spawns the re-scan.
pub fn stop_or_rescanned(
    shutdown: impl Future<Output = ()> + Send + 'static,
    rescan: Option<Rescan>,
) -> (impl Future<Output = ()> + Send + 'static, StopCause) {
    let (ready_tx, ready_rx) = oneshot::channel();
    // Dropping the stop future, wherever serving ends, aborts the re-scan.
    let rescan = rescan.map(|rescan| {
        tokio_util::task::AbortOnDropHandle::new(tokio::spawn(rescan.run(rescan_waits(), ready_tx)))
    });
    let rescanned = Arc::new(AtomicBool::new(false));
    let cause = StopCause(Arc::clone(&rescanned));
    let stop = async move {
        // With no re-scan, the sender went with the closure that never ran,
        // so the receiver errs at once and this never resolves.
        let ready = async {
            if ready_rx.await.is_ok() {
                return;
            }
            std::future::pending::<()>().await;
        };
        tokio::select! {
            () = shutdown => {}
            () = ready => rescanned.store(true, Ordering::SeqCst),
        }
        drop(rescan);
    };
    (stop, cause)
}

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
mod tests {
    use super::*;
    use std::sync::atomic::AtomicUsize;

    #[test]
    fn rescans_wait_10_20_40_then_60_seconds() {
        let waits: Vec<u64> = rescan_waits().take(6).map(|w| w.as_secs()).collect();
        assert_eq!(waits, vec![10, 20, 40, 60, 60, 60]);
    }

    /// Run `rescan` for up to `budget` of paused time, and say whether it
    /// reported a rebuild would serve the list.
    async fn ready_within(rescan: Rescan, budget: Duration) -> bool {
        let (ready_tx, mut ready_rx) = oneshot::channel();
        let _ = tokio::time::timeout(budget, rescan.run(rescan_waits(), ready_tx)).await;
        ready_rx.try_recv().is_ok()
    }

    #[tokio::test(start_paused = true)]
    async fn a_try_with_nothing_in_the_way_says_the_server_can_rebuild() {
        let rescan = Rescan::checking("ZWO", || None);
        assert!(ready_within(rescan, Duration::from_secs(300)).await);
    }

    #[tokio::test(start_paused = true)]
    async fn a_rescan_never_asks_for_a_rebuild_while_something_blocks_it() {
        for blocker in [
            Blocker::Scan("no sysfs".to_string()),
            Blocker::Sdk("no SDK".to_string()),
        ] {
            let rescan = Rescan::checking("ZWO", move || Some(blocker.clone()));
            assert!(
                !ready_within(rescan, Duration::from_secs(300)).await,
                "a blocked re-scan asked for a rebuild"
            );
        }
    }

    #[tokio::test(start_paused = true)]
    async fn a_rescan_tries_after_each_wait_until_nothing_blocks_it() {
        let tries = Arc::new(AtomicUsize::new(0));
        let counted = Arc::clone(&tries);
        let rescan = Rescan::checking("ZWO", move || {
            let n = counted.fetch_add(1, Ordering::SeqCst);
            (n < 2).then(|| Blocker::Scan("still failing".to_string()))
        });
        // 10 s + 20 s + 40 s: the third try is the first to pass.
        assert!(!ready_within(rescan.clone(), Duration::from_secs(69)).await);
        tries.store(0, Ordering::SeqCst);
        assert!(ready_within(rescan, Duration::from_secs(71)).await);
        assert_eq!(tries.load(Ordering::SeqCst), 3);
    }

    /// A re-scan whose scan and enumeration answer `scan` and `enumerate`,
    /// and the log of which of the two each try ran, in order.
    fn recorded(
        scan: Result<(), String>,
        enumerate: Result<(), String>,
    ) -> (Rescan, Arc<std::sync::Mutex<Vec<&'static str>>>) {
        let ran = Arc::new(std::sync::Mutex::new(Vec::new()));
        let (scans, enumerates) = (Arc::clone(&ran), Arc::clone(&ran));
        let rescan = Rescan::new(
            "ZWO",
            move || {
                scans.lock().unwrap().push("scan");
                scan.clone()
            },
            move || {
                enumerates.lock().unwrap().push("enumerate");
                enumerate.clone()
            },
        );
        (rescan, ran)
    }

    #[test]
    fn a_try_with_the_scan_and_the_sdk_working_is_not_blocked() {
        let (rescan, ran) = recorded(Ok(()), Ok(()));
        assert_eq!(rescan.check(), None);
        assert_eq!(*ran.lock().unwrap(), ["scan", "enumerate"]);
    }

    #[test]
    fn a_failed_scan_blocks_before_the_sdk_enumerates() {
        let (rescan, ran) = recorded(Err("no sysfs".to_string()), Ok(()));
        assert_eq!(rescan.check(), Some(Blocker::Scan("no sysfs".to_string())));
        assert_eq!(*ran.lock().unwrap(), ["scan"]);
    }

    #[test]
    fn an_sdk_that_cannot_enumerate_blocks_the_rebuild() {
        let (rescan, ran) = recorded(Ok(()), Err("no SDK".to_string()));
        assert_eq!(rescan.check(), Some(Blocker::Sdk("no SDK".to_string())));
        assert_eq!(*ran.lock().unwrap(), ["scan", "enumerate"]);
    }

    #[test]
    fn a_blocker_is_news_each_time_it_starts() {
        let scan = || Blocker::Scan("no sysfs".to_string());
        let sdk = || Blocker::Sdk("no SDK".to_string());
        assert!(sdk().is_new(None), "the first try's");
        assert!(!sdk().is_new(Some(&sdk())), "one that persists");
        assert!(
            sdk().is_new(Some(&scan())),
            "one that clears and comes back"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn a_server_with_no_rescan_stops_when_told() {
        let (stop, cause) = stop_or_rescanned(async {}, None);
        stop.await;
        assert_eq!(cause.ended(), Ended::Stopped);
    }

    #[tokio::test(start_paused = true)]
    async fn a_server_with_no_rescan_is_never_ended_as_rescanned() {
        let (stop, cause) = stop_or_rescanned(std::future::pending(), None);
        assert!(
            tokio::time::timeout(Duration::from_secs(300), stop)
                .await
                .is_err(),
            "a server with no re-scan stopped on its own"
        );
        assert_eq!(cause.ended(), Ended::Stopped);
    }

    #[tokio::test(start_paused = true)]
    async fn a_rescan_that_succeeds_ends_its_server_as_rescanned() {
        let (stop, cause) = stop_or_rescanned(
            std::future::pending(),
            Some(Rescan::checking("ZWO", || None)),
        );
        tokio::time::timeout(Duration::from_secs(300), stop)
            .await
            .expect("the re-scan's success did not end the server");
        assert_eq!(cause.ended(), Ended::Rescanned);
    }

    #[tokio::test(start_paused = true)]
    async fn a_stop_before_the_rescan_succeeds_ends_it_as_stopped_and_ends_the_rescan() {
        let tries = Arc::new(AtomicUsize::new(0));
        let counted = Arc::clone(&tries);
        let (stop, cause) = stop_or_rescanned(
            tokio::time::sleep(Duration::from_secs(5)),
            Some(Rescan::checking("ZWO", move || {
                counted.fetch_add(1, Ordering::SeqCst);
                None
            })),
        );
        stop.await;
        assert_eq!(cause.ended(), Ended::Stopped);
        // The re-scan was aborted with the stop: no try runs after it.
        tokio::time::sleep(Duration::from_secs(300)).await;
        assert_eq!(tries.load(Ordering::SeqCst), 0);
    }
}
