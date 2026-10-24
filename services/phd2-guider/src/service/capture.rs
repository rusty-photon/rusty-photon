//! The camera facade's capture: one PHD2 frame saved to a file the
//! facade names, the wait for PHD2's `SingleFrameComplete`, the file's
//! read and delete, the orphan sweep, and the restart marker.
//!
//! Contract: `docs/services/phd2-guider.md` § "Camera facade" (sections
//! "Capturing a frame", "Files" and "Restart"). The capture slot it
//! claims lives in [`GuiderOps`], beside the lock the guiding mutations
//! take.

use std::io::Read;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};
use tokio::sync::broadcast;
use tracing::{debug, warn};

use crate::client::Phd2Client;
use crate::config::MAX_CAPTURE_GRACE;
use crate::error::Phd2Error;
use crate::events::AppState;

use super::guider::{
    CaptureClaim, FrameReport, GuiderOps, RpcFailure, StoppedLockError, LOCKED_RPC_TIMEOUT,
};

/// Every file the facade asks PHD2 to save starts with this. The sweep
/// keys on it, so it never touches a file the facade did not name.
pub const FILE_PREFIX: &str = "phd2-guider-";
/// Every file the facade asks PHD2 to save ends with this.
pub const FILE_SUFFIX: &str = ".fits";
/// The restart marker, in `image_dir`. Not a frame name: the sweep
/// leaves it alone.
pub const MARKER_NAME: &str = "phd2-guider-capture.json";
/// The file startup creates and removes to prove `image_dir` writable.
pub const PROBE_NAME: &str = ".phd2-guider-probe";
/// At most this many directory entries are looked at per sweep.
const SWEEP_ENTRY_BUDGET: usize = 4096;
/// FITS header blocks a frame may carry beyond its data. PHD2 writes
/// one; the cap only has to stop an oversized file being read.
const HEADER_BLOCKS_ALLOWED: u64 = 8;
const FITS_BLOCK: u64 = 2880;
/// How long an abort waits for PHD2 to report the frame ended.
pub const ABORT_WAIT: Duration = Duration::from_secs(2);
/// PHD2's longest single frame (`capture_single_frame` refuses more).
pub const MAX_EXPOSURE: Duration = Duration::from_secs(600);

/// Why a capture could not start. Each maps to an ASCOM error in
/// `camera.rs`.
#[derive(Debug, thiserror::Error)]
pub enum StartError {
    /// The capture slot or the guiding lock is taken.
    #[error("{0}")]
    Busy(String),
    /// PHD2 is not in the one state a capture is allowed from.
    #[error("PHD2 is {0}; the camera facade captures only while PHD2 is Stopped")]
    NotStopped(AppState),
    /// PHD2 refused the capture because it is already capturing.
    #[error("PHD2 refused the capture: {0}")]
    Phd2Busy(String),
    /// PHD2 refused the capture for another reason.
    #[error("PHD2 refused the capture: {0}")]
    Phd2Refused(String),
    /// PHD2 could not be reached.
    #[error("PHD2 unreachable: {0}")]
    Unreachable(String),
    /// The facade could not write its restart marker.
    #[error("cannot write the capture marker {path}: {reason}", path = .0.display(), reason = .1)]
    Marker(PathBuf, String),
    /// The request may have reached PHD2, but no answer came back. The
    /// slot stays held until the frame is reported or its deadline.
    #[error(
        "PHD2 did not answer the capture request ({0}); guiding stays refused until the frame \
         is reported or its deadline passes"
    )]
    Unconfirmed(String),
}

/// A frame PHD2 accepted, not yet reported.
#[derive(Debug)]
pub struct InFlight {
    pub path: PathBuf,
    deadline: tokio::time::Instant,
    frames: broadcast::Receiver<FrameReport>,
}

/// How a frame ended.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FrameEnd {
    /// PHD2 saved the frame at the path.
    Saved,
    /// PHD2 reported a failure. `stopped` when this service's stop or
    /// abort caused it.
    Failed { message: String, stopped: bool },
    /// PHD2 did not report the frame by its deadline.
    NotReported { waited: Duration },
}

/// A decoded frame: row-major unsigned 16-bit values.
#[derive(Debug, Clone)]
pub struct Frame {
    pub width: usize,
    pub height: usize,
    pub pixels: Vec<u16>,
}

/// The restart marker's content.
#[derive(Debug, Clone, Serialize, Deserialize)]
struct Marker {
    path: PathBuf,
    deadline_unix_ms: u64,
}

/// How many frames given up on are remembered for a late report.
const ABANDONED_REMEMBERED: usize = 64;

pub struct CaptureEngine {
    ops: Arc<GuiderOps>,
    client: Arc<Phd2Client>,
    image_dir: PathBuf,
    grace: Duration,
    /// Frames whose deadline passed unreported, newest last. A late
    /// `SingleFrameComplete` for one of them deletes its file; the
    /// sweep catches any this misses.
    abandoned: std::sync::Mutex<std::collections::VecDeque<PathBuf>>,
}

impl CaptureEngine {
    pub const fn new(
        ops: Arc<GuiderOps>,
        client: Arc<Phd2Client>,
        image_dir: PathBuf,
        grace: Duration,
    ) -> Self {
        Self {
            ops,
            client,
            image_dir,
            grace,
            abandoned: std::sync::Mutex::new(std::collections::VecDeque::new()),
        }
    }

    fn abandoned(&self) -> std::sync::MutexGuard<'_, std::collections::VecDeque<PathBuf>> {
        self.abandoned
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    pub const fn grace(&self) -> Duration {
        self.grace
    }

    /// The guiding operations the capture slot lives beside.
    pub const fn ops(&self) -> &Arc<GuiderOps> {
        &self.ops
    }

    /// Start one exposure of `exposure` (whole milliseconds, 1 to
    /// 600 000): claim the capture slot under the guiding lock, check
    /// PHD2 is Stopped, write the marker, and ask PHD2 for the frame.
    /// The lock is held only for these steps; the exposure holds none.
    /// The orphan sweep is the caller's, once it has answered its client
    /// ([`Self::sweep_orphans`]).
    ///
    /// # Errors
    ///
    /// Returns [`StartError`] when the slot or the lock is taken, PHD2
    /// is not Stopped or refuses the capture, PHD2 cannot be reached, or
    /// the marker cannot be written; nothing is left claimed then. When
    /// the request may have reached PHD2 but no answer came back, the
    /// error is [`StartError::Unconfirmed`], and the slot stays held
    /// until PHD2 reports the frame or its deadline passes, as for a
    /// frame PHD2 accepted.
    pub async fn start(self: &Arc<Self>, exposure_ms: u32) -> Result<InFlight, StartError> {
        let op = self.ops.lock_while_stopped().await.map_err(|e| match e {
            e @ (StoppedLockError::Busy | StoppedLockError::Capturing(_)) => {
                StartError::Busy(e.to_string())
            }
            StoppedLockError::Rpc(failure) => start_error_of(failure),
            StoppedLockError::NotStopped(state) => StartError::NotStopped(state),
        })?;

        let path = self.image_dir.join(format!(
            "{FILE_PREFIX}{}{FILE_SUFFIX}",
            uuid::Uuid::new_v4().simple()
        ));
        // `bind_camera` refused a non-UTF-8 `image_dir` at startup.
        let path_text = path.to_str().unwrap_or_default().to_string();
        let total = Duration::from_millis(u64::from(exposure_ms)).saturating_add(self.grace);
        let deadline_wall = SystemTime::now()
            .checked_add(total)
            .unwrap_or_else(SystemTime::now);
        let deadline = tokio::time::Instant::now()
            .checked_add(total)
            .unwrap_or_else(tokio::time::Instant::now);
        let marker_path = self.marker_path();
        write_marker(
            &marker_path,
            &Marker {
                path: path.clone(),
                deadline_unix_ms: unix_ms(deadline_wall),
            },
        )
        .map_err(|e| StartError::Marker(marker_path.clone(), e.to_string()))?;

        // Subscribe before the RPC, so a frame that ends at once cannot
        // report before anyone listens.
        let frames = self.ops.subscribe_frames();
        self.ops.claim_capture(
            &op,
            CaptureClaim {
                path: path.clone(),
                expires: deadline,
                deadline: deadline_wall,
                stop_requested: false,
                recovered: false,
            },
        );
        let inflight = InFlight {
            path: path.clone(),
            deadline,
            frames,
        };
        let request = tokio::time::timeout(
            LOCKED_RPC_TIMEOUT,
            self.client.capture_single_frame_to(exposure_ms, &path_text),
        )
        .await
        .unwrap_or_else(|_| {
            Err(Phd2Error::Timeout(
                "capture_single_frame got no answer".to_string(),
            ))
        });
        let inflight = self.settle_request(request, inflight)?;
        drop(op);
        debug!(path = %path.display(), exposure_ms, "PHD2 accepted the facade's frame");
        Ok(inflight)
    }

    /// Delete every orphaned facade frame in `image_dir` but `keep`, the
    /// capture in flight. Holds no lock.
    pub async fn sweep_orphans(&self, keep: &Path) {
        let image_dir = self.image_dir.clone();
        let keep = keep.to_path_buf();
        let removed = tokio::task::spawn_blocking(move || sweep(&image_dir, Some(&keep)))
            .await
            .unwrap_or_default();
        if removed > 0 {
            debug!("swept {removed} orphaned frame file(s) as the capture started");
        }
    }

    /// What PHD2's answer to `capture_single_frame` means for the frame
    /// `inflight` claims. Called with the guiding lock held.
    ///
    /// # Errors
    ///
    /// A refusal, or a request never sent, releases the slot and removes
    /// the marker: nothing started. Any other failure means the request
    /// may have reached PHD2, so the slot and the marker stay, held by a
    /// background wait as for a frame PHD2 accepted
    /// ([`StartError::Unconfirmed`]). That includes a reply this client
    /// cannot read: it may be an acceptance, and guiding sent into a
    /// running frame can wedge PHD2.
    fn settle_request(
        self: &Arc<Self>,
        request: crate::error::Result<()>,
        inflight: InFlight,
    ) -> Result<InFlight, StartError> {
        let path = inflight.path.clone();
        match request {
            Ok(()) => Ok(inflight),
            Err(e @ (Phd2Error::RpcError { .. } | Phd2Error::NotConnected)) => {
                remove_marker_naming(&self.marker_path(), &path);
                self.ops.release_capture(&path);
                Err(map_rpc_error(e))
            }
            Err(e) => {
                warn!(
                    path = %path.display(),
                    "PHD2 did not answer the capture request ({e}); guiding stays refused until \
                     the frame is reported or its deadline passes"
                );
                let engine = Arc::clone(self);
                tokio::spawn(async move {
                    // A frame given up on has no file yet; its late report
                    // deletes it. Any other end may have left one.
                    if !matches!(engine.wait(inflight).await, FrameEnd::NotReported { .. }) {
                        engine.discard(&path);
                    }
                });
                Err(StartError::Unconfirmed(e.to_string()))
            }
        }
    }

    /// Wait for PHD2 to report `inflight`'s frame, or for its deadline,
    /// then release the capture slot and remove the marker. The file, if
    /// any, is the caller's to read and must then be removed with
    /// [`Self::discard`].
    pub async fn wait(&self, mut inflight: InFlight) -> FrameEnd {
        let started = tokio::time::Instant::now();
        let mut expired = std::pin::pin!(tokio::time::sleep_until(inflight.deadline));
        let end = loop {
            tokio::select! {
                // A report that is already there wins over the deadline.
                biased;
                report = inflight.frames.recv() => match report {
                    Ok(report) if report_names(&report, &inflight.path) => {
                        break self.frame_end(report, &inflight.path);
                    }
                    Ok(_) => {}
                    Err(broadcast::error::RecvError::Lagged(n)) => {
                        debug!("frame wait lagged, skipped {n} reports");
                    }
                    Err(broadcast::error::RecvError::Closed) => {
                        // The service is shutting down; the deadline
                        // still bounds the wait.
                        (&mut expired).await;
                        break FrameEnd::NotReported { waited: whole_ms(started.elapsed()) };
                    }
                },
                () = &mut expired => {
                    break FrameEnd::NotReported { waited: whole_ms(started.elapsed()) };
                }
            }
        };
        let given_up = matches!(end, FrameEnd::NotReported { .. });
        if let FrameEnd::NotReported { waited } = &end {
            warn!(
                path = %inflight.path.display(),
                "PHD2 did not report the facade's frame within {}; releasing the capture slot",
                humantime::format_duration(*waited)
            );
        }
        self.finish_claim(&inflight.path, given_up).await;
        end
    }

    fn frame_end(&self, report: FrameReport, path: &Path) -> FrameEnd {
        if report.success {
            return FrameEnd::Saved;
        }
        let stopped = self
            .ops
            .capture_claim()
            .is_some_and(|claim| claim.path == path && claim.stop_requested);
        FrameEnd::Failed {
            message: report
                .error
                .unwrap_or_else(|| "no reason given".to_string()),
            stopped,
        }
    }

    /// End the claim of the frame at `path`, under the guiding lock so no
    /// new capture can write its marker meanwhile: remember the frame as
    /// given up on (`given_up`), remove the marker if it still names this
    /// frame, and release the slot.
    async fn finish_claim(&self, path: &Path, given_up: bool) {
        // Before the lock: a late report that lands while this waits for
        // it must already find the frame given up on.
        if given_up {
            let mut abandoned = self.abandoned();
            if abandoned.len() == ABANDONED_REMEMBERED {
                abandoned.pop_front();
            }
            abandoned.push_back(path.to_path_buf());
        }
        let _op = self.ops.op_lock().lock().await;
        remove_marker_naming(&self.marker_path(), path);
        self.ops.release_capture(path);
    }

    /// Read and decode a saved frame, checking it against the frame
    /// size read on connect. Bounded: the file is opened without
    /// following a link, must be a regular file, and may be no larger
    /// than the frame needs.
    ///
    /// # Errors
    ///
    /// Returns a message for an unreadable, oversized or malformed file,
    /// or one whose size differs from `width` × `height`.
    pub async fn read(&self, path: &Path, width: u32, height: u32) -> Result<Frame, String> {
        let path = path.to_path_buf();
        tokio::task::spawn_blocking(move || read_frame(&path, width, height))
            .await
            .map_err(|e| format!("frame read task failed: {e}"))?
    }

    /// Delete a frame file. A failure is logged, never returned.
    pub fn discard(&self, path: &Path) {
        if remove_quietly(path) {
            debug!(path = %path.display(), "deleted the facade's frame file");
        }
    }

    /// End a frame this facade started: send PHD2 `stop_capture` if the
    /// frame still holds the slot, then wait up to `limit` for PHD2 to
    /// report it ended. Returns whether it did.
    pub async fn abort(&self, path: &Path, limit: Duration) -> bool {
        {
            let _op = self.ops.op_lock().lock().await;
            let ours = self
                .ops
                .capture_claim()
                .is_some_and(|claim| claim.path == path && !claim.recovered);
            if !ours {
                return true;
            }
            self.ops.mark_capture_stopped();
            match tokio::time::timeout(LOCKED_RPC_TIMEOUT, self.client.stop_capture()).await {
                Ok(Ok(())) => {}
                Ok(Err(e)) => warn!("cannot stop PHD2's frame: {e}"),
                Err(_) => warn!(
                    "PHD2 did not answer stop_capture within {}",
                    humantime::format_duration(LOCKED_RPC_TIMEOUT)
                ),
            }
        }
        self.ops.wait_capture_released(path, limit).await
    }

    /// Delete files left by frames the facade gave up on, whenever PHD2
    /// reports one late, and release a restart marker's frame when PHD2
    /// reports it. The task holds the engine only while it handles a
    /// report, so it does not keep the engine (and with it the event
    /// stream) alive: it ends at the first report after the engine is
    /// dropped, or when the stream closes.
    pub fn spawn_late_file_cleaner(self: &Arc<Self>) {
        let engine = Arc::downgrade(self);
        let mut frames = self.ops.subscribe_frames();
        tokio::spawn(async move {
            loop {
                let report = match frames.recv().await {
                    Ok(report) => report,
                    Err(broadcast::error::RecvError::Lagged(n)) => {
                        debug!("late-file cleaner lagged, skipped {n} reports");
                        continue;
                    }
                    Err(broadcast::error::RecvError::Closed) => break,
                };
                let Some(engine) = engine.upgrade() else {
                    break;
                };
                engine.on_late_report(&report).await;
            }
        });
    }

    async fn on_late_report(&self, report: &FrameReport) {
        let Some(reported) = report.path.as_deref().map(Path::new) else {
            return;
        };
        if !self.is_our_frame(reported) {
            return;
        }
        // A frame given up on at its deadline. Only those: a live
        // capture's own report races this one, and its waiter reads the
        // file before deleting it.
        let given_up = {
            let mut abandoned = self.abandoned();
            abandoned
                .iter()
                .position(|path| path == reported)
                .and_then(|at| abandoned.remove(at))
                .is_some()
        };
        if given_up {
            debug!(path = %reported.display(), "PHD2 saved a frame the facade gave up on");
            self.discard(reported);
            return;
        }
        if self
            .ops
            .capture_claim()
            .is_some_and(|claim| claim.path == reported && claim.recovered)
        {
            debug!(path = %reported.display(), "PHD2 reported the restart marker's frame");
            self.discard(reported);
            self.finish_claim(reported, false).await;
        }
    }

    fn is_our_frame(&self, path: &Path) -> bool {
        path.parent() == Some(self.image_dir.as_path())
            && path
                .file_name()
                .and_then(|name| name.to_str())
                .is_some_and(is_frame_name)
    }

    /// At startup: sweep orphans, and honour a restart marker. A marker
    /// whose deadline is still ahead holds the capture slot until PHD2
    /// reports its frame or the deadline passes; one whose deadline
    /// passed, or that names no facade frame, is removed at once. A
    /// deadline further ahead than the longest exposure plus the longest
    /// grace is clamped to that and written back, so a marker written
    /// under a wrong clock cannot hold the slot for longer than any frame
    /// could, however often the service restarts.
    pub async fn recover(self: &Arc<Self>) {
        let marker_path = self.marker_path();
        let marker = match std::fs::read(&marker_path) {
            Ok(bytes) => match serde_json::from_slice::<Marker>(&bytes) {
                Ok(marker) => Some(marker),
                Err(e) => {
                    warn!(path = %marker_path.display(), "unreadable capture marker, removed: {e}");
                    remove_quietly(&marker_path);
                    None
                }
            },
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => None,
            Err(e) => {
                warn!(path = %marker_path.display(), "cannot read the capture marker: {e}");
                None
            }
        };
        let now = SystemTime::now();
        let latest = now
            .checked_add(MAX_EXPOSURE.saturating_add(MAX_CAPTURE_GRACE))
            .unwrap_or(now);
        let written = marker
            .as_ref()
            .and_then(|m| UNIX_EPOCH.checked_add(Duration::from_millis(m.deadline_unix_ms)));
        let in_flight = match (&marker, written) {
            (Some(marker), _) if !self.is_our_frame(&marker.path) => {
                warn!(
                    path = %marker.path.display(),
                    "capture marker names no camera-facade frame; removing it"
                );
                remove_quietly(&marker_path);
                None
            }
            (Some(marker), Some(written)) if written > now => {
                let deadline = written.min(latest);
                if deadline < written {
                    // Written back, so the next start does not clamp
                    // again from its own startup time.
                    let clamped = Marker {
                        path: marker.path.clone(),
                        deadline_unix_ms: unix_ms(deadline),
                    };
                    if let Err(e) = write_marker(&marker_path, &clamped) {
                        warn!(path = %marker_path.display(), "cannot rewrite the capture marker: {e}");
                    }
                }
                Some((marker.path.clone(), deadline))
            }
            (Some(marker), _) => {
                debug!("capture marker's deadline has passed; removing it");
                remove_quietly(&marker_path);
                self.discard(&marker.path);
                None
            }
            (None, _) => None,
        };
        let keep = in_flight.as_ref().map(|(path, _)| path.clone());
        let image_dir = self.image_dir.clone();
        let removed = tokio::task::spawn_blocking(move || sweep(&image_dir, keep.as_deref()))
            .await
            .unwrap_or_default();
        if removed > 0 {
            debug!("removed {removed} orphaned camera-facade frame file(s) at startup");
        }
        let Some((path, deadline)) = in_flight else {
            return;
        };
        // From here the slot runs on the monotonic clock, as a frame this
        // process started does.
        let left = deadline.duration_since(now).unwrap_or_default();
        let expires = tokio::time::Instant::now()
            .checked_add(left)
            .unwrap_or_else(tokio::time::Instant::now);
        {
            let op = self.ops.op_lock().lock().await;
            self.ops.claim_capture(
                &op,
                CaptureClaim {
                    path: path.clone(),
                    expires,
                    deadline,
                    stop_requested: false,
                    recovered: true,
                },
            );
        }
        warn!(
            path = %path.display(),
            "a camera-facade frame may still be exposing from before the restart; guiding \
             operations are refused until {} at the latest",
            humantime::format_rfc3339_seconds(deadline)
        );
        let engine = Arc::clone(self);
        tokio::spawn(async move { engine.watch_recovered(path, expires).await });
    }

    /// Hold the restart marker's frame until PHD2 reports it (the late
    /// cleaner then releases it) or it expires. The frame's file existing
    /// proves nothing: PHD2 creates it before it has written it.
    async fn watch_recovered(&self, path: PathBuf, expires: tokio::time::Instant) {
        let left = expires.saturating_duration_since(tokio::time::Instant::now());
        if self.ops.wait_capture_released(&path, left).await {
            return; // PHD2 reported it; the cleaner released it.
        }
        debug!(path = %path.display(), "the restart marker's frame reached its deadline");
        self.discard(&path);
        self.finish_claim(&path, false).await;
    }

    /// At shutdown: end the facade's frame in flight, if any, and wait
    /// up to the capture grace for PHD2 to report it.
    pub async fn shutdown(&self) {
        let Some(claim) = self.ops.capture_claim() else {
            return;
        };
        if claim.recovered {
            return;
        }
        debug!(path = %claim.path.display(), "shutdown ends the facade's frame in flight");
        if !self.abort(&claim.path, self.grace).await {
            warn!(
                path = %claim.path.display(),
                "PHD2 did not report the facade's frame before shutdown"
            );
        }
        self.discard(&claim.path);
    }

    fn marker_path(&self) -> PathBuf {
        self.image_dir.join(MARKER_NAME)
    }
}

/// Check that `image_dir` is an existing directory the service can
/// create and remove files in, by creating and removing [`PROBE_NAME`].
///
/// Startup runs it, and so does a setup-page save, inside the service,
/// so both test the permissions the service has.
///
/// # Errors
///
/// Returns a message naming the directory and the failure; the caller
/// names the field.
pub fn check_image_dir(image_dir: &Path) -> Result<(), String> {
    let meta = std::fs::metadata(image_dir)
        .map_err(|e| format!("{} cannot be used: {e}", image_dir.display()))?;
    if !meta.is_dir() {
        return Err(format!("{} is not a directory", image_dir.display()));
    }
    let probe = image_dir.join(PROBE_NAME);
    std::fs::write(&probe, b"").map_err(|e| {
        format!(
            "{} is not writable by this service: {e}",
            image_dir.display()
        )
    })?;
    std::fs::remove_file(&probe)
        .map_err(|e| format!("{}: cannot remove {PROBE_NAME}: {e}", image_dir.display()))?;
    Ok(())
}

fn report_names(report: &FrameReport, path: &Path) -> bool {
    report.path.as_deref().map(Path::new) == Some(path)
}

/// `phd2-guider-` + 32 lowercase hex digits + `.fits`: exactly the names
/// `start` makes, so the sweep deletes nothing else.
fn is_frame_name(name: &str) -> bool {
    name.strip_prefix(FILE_PREFIX)
        .and_then(|rest| rest.strip_suffix(FILE_SUFFIX))
        .is_some_and(|hex| {
            hex.len() == 32 && hex.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'))
        })
}

/// Delete every facade frame file in `image_dir` except `keep`, looking
/// at no more than [`SWEEP_ENTRY_BUDGET`] entries. Returns how many were
/// deleted.
fn sweep(image_dir: &Path, keep: Option<&Path>) -> usize {
    let entries = match std::fs::read_dir(image_dir) {
        Ok(entries) => entries,
        Err(e) => {
            warn!(dir = %image_dir.display(), "cannot sweep camera.image_dir: {e}");
            return 0;
        }
    };
    let mut removed: usize = 0;
    let mut looked: usize = 0;
    for entry in entries.flatten() {
        looked = looked.saturating_add(1);
        if looked > SWEEP_ENTRY_BUDGET {
            warn!(
                dir = %image_dir.display(),
                "sweep stopped after {SWEEP_ENTRY_BUDGET} entries; the rest wait for the next one"
            );
            break;
        }
        let name = entry.file_name();
        let Some(name) = name.to_str() else { continue };
        if !is_frame_name(name) {
            continue;
        }
        let path = entry.path();
        if keep == Some(path.as_path()) {
            continue;
        }
        match std::fs::remove_file(&path) {
            Ok(()) => {
                debug!(path = %path.display(), "swept an orphaned frame file");
                removed = removed.saturating_add(1);
            }
            Err(e) => warn!(path = %path.display(), "cannot sweep orphaned frame file: {e}"),
        }
    }
    removed
}

/// The largest file a `width` × `height` frame may be: its 16-bit data
/// rounded up to whole FITS blocks, plus [`HEADER_BLOCKS_ALLOWED`]
/// header blocks.
fn frame_size_cap(width: u32, height: u32) -> u64 {
    let data = u64::from(width)
        .saturating_mul(u64::from(height))
        .saturating_mul(2);
    data.div_ceil(FITS_BLOCK)
        .saturating_add(HEADER_BLOCKS_ALLOWED)
        .saturating_mul(FITS_BLOCK)
}

fn read_frame(path: &Path, width: u32, height: u32) -> Result<Frame, String> {
    let mut file = open_no_follow(path)
        .map_err(|e| format!("cannot open PHD2's frame {}: {e}", path.display()))?;
    let meta = file
        .metadata()
        .map_err(|e| format!("cannot inspect PHD2's frame {}: {e}", path.display()))?;
    if meta.file_type().is_symlink() || !meta.is_file() {
        return Err(format!(
            "PHD2's frame {} is not a regular file",
            path.display()
        ));
    }
    let cap = frame_size_cap(width, height);
    if meta.len() > cap {
        return Err(format!(
            "PHD2's frame {} is {} bytes, more than a {width} x {height} frame needs ({cap})",
            path.display(),
            meta.len()
        ));
    }
    let mut bytes = Vec::new();
    file.by_ref()
        .take(cap.saturating_add(1))
        .read_to_end(&mut bytes)
        .map_err(|e| format!("cannot read PHD2's frame {}: {e}", path.display()))?;
    if u64::try_from(bytes.len()).unwrap_or(u64::MAX) > cap {
        return Err(format!(
            "PHD2's frame {} grew past {cap} bytes while it was read",
            path.display()
        ));
    }
    let image = rp_fits::reader::read_primary(std::io::Cursor::new(bytes)).map_err(|e| {
        format!(
            "PHD2's frame {} is not a readable FITS image: {e}",
            path.display()
        )
    })?;
    let (got_width, got_height) = (image.width, image.height);
    let expected = (
        usize::try_from(width).unwrap_or(usize::MAX),
        usize::try_from(height).unwrap_or(usize::MAX),
    );
    if (got_width, got_height) != expected {
        return Err(format!(
            "PHD2's frame size changed: {got_width} x {got_height}, not the {width} x {height} \
             read on connect; reconnect the camera"
        ));
    }
    let pixels = unsigned_16(image).map_err(|why| {
        format!(
            "PHD2's frame {} {why}, so it is not the unsigned 16-bit frame PHD2 writes",
            path.display()
        )
    })?;
    Ok(Frame {
        width: got_width,
        height: got_height,
        pixels,
    })
}

/// A frame's values as unsigned 16-bit. PHD2 writes `BITPIX = 16` with
/// `BZERO = 32768`, which maps each stored value to its unsigned one with
/// no arithmetic and no wider copy of the frame. A frame of integers in
/// another encoding is scaled in integer arithmetic, so a value is never
/// truncated or clamped: it is a whole number from 0 to 65535, or the
/// frame is refused.
///
/// # Errors
///
/// Says what is wrong: floating-point values, a `BSCALE` or `BZERO` that
/// is not a whole number, or a value outside 0 to 65535.
fn unsigned_16(image: rp_fits::reader::FitsImage) -> Result<Vec<u16>, String> {
    use rp_fits::reader::Pixels;
    let rp_fits::reader::FitsImage {
        data,
        bscale,
        bzero,
        ..
    } = image;
    let (Some(scale), Some(zero)) = (whole(bscale), whole(bzero)) else {
        return Err(format!(
            "scales its values by BSCALE {bscale} and BZERO {bzero}, which are not both whole \
             numbers"
        ));
    };
    let value = |stored: i64| {
        stored
            .checked_mul(scale)
            .and_then(|scaled| scaled.checked_add(zero))
            .and_then(|value| u16::try_from(value).ok())
    };
    let values: Option<Vec<u16>> = match data {
        Pixels::I16(stored) if (scale, zero) == (1, 32768) => {
            return Ok(stored
                .into_iter()
                .map(|value| value.cast_unsigned() ^ 0x8000)
                .collect());
        }
        Pixels::U8(stored) => stored.into_iter().map(|v| value(i64::from(v))).collect(),
        Pixels::I16(stored) => stored.into_iter().map(|v| value(i64::from(v))).collect(),
        Pixels::I32(stored) => stored.into_iter().map(|v| value(i64::from(v))).collect(),
        Pixels::I64(stored) => stored.into_iter().map(value).collect(),
        Pixels::F32(_) | Pixels::F64(_) => {
            return Err("holds floating-point values (BITPIX -32 or -64)".to_string());
        }
    };
    values.ok_or_else(|| "holds a value outside 0 to 65535".to_string())
}

/// `value` as an integer, when it is a whole number an `i64` holds. No
/// `TryFrom<f64>` exists. `f64`'s `Display` writes a whole number as its
/// exact digits, with no exponent and no fraction, while a fraction, NaN
/// and the infinities write text that does not parse as one.
fn whole(value: f64) -> Option<i64> {
    value.to_string().parse().ok()
}

#[cfg(unix)]
fn open_no_follow(path: &Path) -> std::io::Result<std::fs::File> {
    use std::os::unix::fs::OpenOptionsExt;
    std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW)
        .open(path)
}

#[cfg(windows)]
fn open_no_follow(path: &Path) -> std::io::Result<std::fs::File> {
    use std::os::windows::fs::OpenOptionsExt;
    /// `FILE_FLAG_OPEN_REPARSE_POINT`: open a link itself, not its target.
    const FILE_FLAG_OPEN_REPARSE_POINT: u32 = 0x0020_0000;
    std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(FILE_FLAG_OPEN_REPARSE_POINT)
        .open(path)
}

#[cfg(not(any(unix, windows)))]
fn open_no_follow(path: &Path) -> std::io::Result<std::fs::File> {
    std::fs::File::open(path)
}

fn write_marker(path: &Path, marker: &Marker) -> std::io::Result<()> {
    let body = serde_json::to_vec(marker).map_err(std::io::Error::other)?;
    let staged = path.with_extension("json.tmp");
    std::fs::write(&staged, body)?;
    std::fs::rename(&staged, path)
}

/// Remove the restart marker if it names the frame at `path`; a marker
/// a later capture wrote is left alone. The callers hold the guiding
/// lock, so no capture writes one meanwhile.
fn remove_marker_naming(marker_path: &Path, path: &Path) {
    let names_it = std::fs::read(marker_path)
        .ok()
        .and_then(|bytes| serde_json::from_slice::<Marker>(&bytes).ok())
        .is_none_or(|marker| marker.path == path);
    if names_it {
        remove_quietly(marker_path);
    }
}

/// Remove a file the facade owns: one already gone is fine, any other
/// failure is logged. Returns whether a file was removed.
fn remove_quietly(path: &Path) -> bool {
    match std::fs::remove_file(path) {
        Ok(()) => true,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => false,
        Err(e) => {
            warn!(path = %path.display(), "cannot remove {}: {e}", path.display());
            false
        }
    }
}

/// `duration` to whole milliseconds, for messages.
fn whole_ms(duration: Duration) -> Duration {
    Duration::from_millis(u64::try_from(duration.as_millis()).unwrap_or(u64::MAX))
}

fn unix_ms(time: SystemTime) -> u64 {
    time.duration_since(UNIX_EPOCH)
        .map_or(0, |d| u64::try_from(d.as_millis()).unwrap_or(u64::MAX))
}

/// Map a PHD2 client error from the capture's RPCs.
fn map_rpc_error(e: Phd2Error) -> StartError {
    start_error_of(e.into())
}

/// A failed request to PHD2, as a capture that could not start.
fn start_error_of(failure: RpcFailure) -> StartError {
    match failure {
        RpcFailure::Busy(message) => StartError::Phd2Busy(message),
        RpcFailure::Refused(message) => StartError::Phd2Refused(message),
        RpcFailure::Unreachable(message) => StartError::Unreachable(message),
    }
}

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
mod tests {
    use super::*;

    use crate::config::{Phd2Config, SettleParams};
    use crate::events::Phd2Event;

    const FRAME: &str = "phd2-guider-0123456789abcdef0123456789abcdef.fits";

    fn engine(image_dir: &Path) -> Arc<CaptureEngine> {
        let client = Arc::new(Phd2Client::new(Phd2Config::default()));
        let ops = Arc::new(GuiderOps::new(
            Arc::clone(&client),
            SettleParams::default(),
            Duration::from_secs(10),
        ));
        Arc::new(CaptureEngine::new(
            ops,
            client,
            image_dir.to_path_buf(),
            Duration::from_secs(10),
        ))
    }

    async fn claim(engine: &CaptureEngine, path: &Path, recovered: bool) {
        let op = engine.ops.op_lock().lock().await;
        engine.ops.claim_capture(
            &op,
            CaptureClaim {
                path: path.to_path_buf(),
                expires: tokio::time::Instant::now() + Duration::from_secs(60),
                deadline: SystemTime::now() + Duration::from_secs(60),
                stop_requested: false,
                recovered,
            },
        );
    }

    fn report(engine: &CaptureEngine, path: &Path, success: bool, error: Option<&str>) {
        engine.ops.ingest(Phd2Event::SingleFrameComplete {
            success,
            error: error.map(str::to_string),
            path: Some(path.to_str().unwrap().to_string()),
        });
    }

    /// A frame in flight as `start` leaves it, without PHD2.
    async fn in_flight(engine: &CaptureEngine, path: &Path, deadline: Duration) -> InFlight {
        let frames = engine.ops.subscribe_frames();
        claim(engine, path, false).await;
        write_marker(
            &engine.marker_path(),
            &Marker {
                path: path.to_path_buf(),
                deadline_unix_ms: unix_ms(SystemTime::now() + deadline),
            },
        )
        .unwrap();
        InFlight {
            path: path.to_path_buf(),
            deadline: tokio::time::Instant::now() + deadline,
            frames,
        }
    }

    #[tokio::test]
    async fn phd2s_report_naming_the_frame_ends_the_wait_and_frees_the_slot() {
        let dir = tempfile::tempdir().unwrap();
        let engine = engine(dir.path());
        let path = dir.path().join(FRAME);
        let inflight = in_flight(&engine, &path, Duration::from_secs(30)).await;
        // Another frame's report is not this one's.
        report(
            &engine,
            &dir.path().join("phd2-guider-other.fits"),
            true,
            None,
        );
        report(&engine, &path, true, None);
        assert_eq!(engine.wait(inflight).await, FrameEnd::Saved);
        assert!(engine.ops.capture_claim().is_none());
        assert!(!engine.marker_path().exists());
    }

    #[tokio::test]
    async fn a_failure_after_a_stop_reads_as_the_stop() {
        let dir = tempfile::tempdir().unwrap();
        let engine = engine(dir.path());
        let path = dir.path().join(FRAME);
        let inflight = in_flight(&engine, &path, Duration::from_secs(30)).await;
        engine.ops.mark_capture_stopped();
        report(&engine, &path, false, Some("camera error -- see debug log"));
        assert_eq!(
            engine.wait(inflight).await,
            FrameEnd::Failed {
                message: "camera error -- see debug log".to_string(),
                stopped: true,
            }
        );
    }

    #[tokio::test]
    async fn a_failure_without_a_stop_carries_phd2s_text() {
        let dir = tempfile::tempdir().unwrap();
        let engine = engine(dir.path());
        let path = dir.path().join(FRAME);
        let inflight = in_flight(&engine, &path, Duration::from_secs(30)).await;
        report(&engine, &path, false, None);
        assert_eq!(
            engine.wait(inflight).await,
            FrameEnd::Failed {
                message: "no reason given".to_string(),
                stopped: false,
            }
        );
    }

    #[tokio::test]
    async fn an_unconfirmed_frame_that_fails_still_has_its_file_deleted() {
        let dir = tempfile::tempdir().unwrap();
        let engine = engine(dir.path());
        let path = dir.path().join(FRAME);
        let inflight = in_flight(&engine, &path, Duration::from_secs(30)).await;
        let err = engine
            .settle_request(Err(Phd2Error::Timeout("no answer".into())), inflight)
            .unwrap_err();
        assert!(matches!(err, StartError::Unconfirmed(_)), "{err:?}");
        assert!(
            engine.ops.capture_claim().is_some(),
            "an unconfirmed frame freed the slot"
        );
        // PHD2 did start the frame, and failed after creating its file.
        std::fs::write(&path, b"partial").unwrap();
        report(&engine, &path, false, Some("error saving image file"));
        assert!(
            engine
                .ops
                .wait_capture_released(&path, Duration::from_secs(5))
                .await
        );
        // On this one-thread runtime the background wait releases the
        // slot and deletes the file in one poll, with no await between.
        assert!(!path.exists(), "a failed frame's file was left behind");
    }

    #[tokio::test(start_paused = true)]
    async fn an_unreported_frame_is_given_up_at_its_deadline_and_remembered() {
        let dir = tempfile::tempdir().unwrap();
        let engine = engine(dir.path());
        let path = dir.path().join(FRAME);
        let inflight = in_flight(&engine, &path, Duration::from_secs(11)).await;
        let end = engine.wait(inflight).await;
        assert!(matches!(end, FrameEnd::NotReported { .. }), "{end:?}");
        assert!(engine.ops.capture_claim().is_none());
        assert!(!engine.marker_path().exists());
        // PHD2 saves it late: the file is deleted when the report lands.
        std::fs::write(&path, b"late").unwrap();
        engine
            .on_late_report(&FrameReport {
                success: true,
                error: None,
                path: Some(path.to_str().unwrap().to_string()),
            })
            .await;
        assert!(!path.exists());
    }

    #[tokio::test]
    async fn a_late_report_leaves_the_live_capture_and_foreign_files_alone() {
        let dir = tempfile::tempdir().unwrap();
        let engine = engine(dir.path());
        let path = dir.path().join(FRAME);
        std::fs::write(&path, b"frame").unwrap();
        claim(&engine, &path, false).await;
        let late = |path: &Path| FrameReport {
            success: true,
            error: None,
            path: Some(path.to_str().unwrap().to_string()),
        };
        engine.on_late_report(&late(&path)).await;
        assert!(path.exists(), "the live capture's file was deleted");
        let foreign = dir.path().join("save_image_abc123");
        std::fs::write(&foreign, b"phd2's").unwrap();
        engine.on_late_report(&late(&foreign)).await;
        assert!(foreign.exists());
        engine
            .on_late_report(&FrameReport {
                success: true,
                error: None,
                path: None,
            })
            .await;
    }

    #[tokio::test]
    async fn a_report_for_the_restart_markers_frame_frees_the_slot() {
        let dir = tempfile::tempdir().unwrap();
        let engine = engine(dir.path());
        let path = dir.path().join(FRAME);
        std::fs::write(&path, b"frame").unwrap();
        std::fs::write(engine.marker_path(), b"{}").unwrap();
        claim(&engine, &path, true).await;
        engine
            .on_late_report(&FrameReport {
                success: true,
                error: None,
                path: Some(path.to_str().unwrap().to_string()),
            })
            .await;
        assert!(engine.ops.capture_claim().is_none());
        assert!(!path.exists());
        assert!(!engine.marker_path().exists());
    }

    fn stage_marker(engine: &CaptureEngine, path: &Path, deadline: SystemTime) {
        write_marker(
            &engine.marker_path(),
            &Marker {
                path: path.to_path_buf(),
                deadline_unix_ms: unix_ms(deadline),
            },
        )
        .unwrap();
    }

    #[tokio::test]
    async fn a_marker_whose_deadline_passed_is_removed_at_startup() {
        let dir = tempfile::tempdir().unwrap();
        let engine = engine(dir.path());
        let path = dir.path().join(FRAME);
        std::fs::write(&path, b"frame").unwrap();
        stage_marker(&engine, &path, SystemTime::now() - Duration::from_secs(5));
        engine.recover().await;
        assert!(engine.ops.capture_claim().is_none());
        assert!(!engine.marker_path().exists());
        assert!(!path.exists());
    }

    #[tokio::test]
    async fn an_unreadable_marker_is_removed_at_startup() {
        let dir = tempfile::tempdir().unwrap();
        let engine = engine(dir.path());
        std::fs::write(engine.marker_path(), b"not json").unwrap();
        engine.recover().await;
        assert!(engine.ops.capture_claim().is_none());
        assert!(!engine.marker_path().exists());
    }

    #[tokio::test]
    async fn a_marker_naming_a_file_outside_the_facades_frames_is_removed() {
        let dir = tempfile::tempdir().unwrap();
        let engine = engine(dir.path());
        let elsewhere = dir.path().join("save_image_abc123");
        std::fs::write(&elsewhere, b"phd2's").unwrap();
        stage_marker(
            &engine,
            &elsewhere,
            SystemTime::now() + Duration::from_secs(60),
        );
        engine.recover().await;
        assert!(engine.ops.capture_claim().is_none());
        assert!(!engine.marker_path().exists());
        assert!(elsewhere.exists());
    }

    #[tokio::test]
    async fn a_live_marker_holds_the_slot_until_phd2_reports_its_frame() {
        let dir = tempfile::tempdir().unwrap();
        let engine = engine(dir.path());
        let path = dir.path().join(FRAME);
        stage_marker(&engine, &path, SystemTime::now() + Duration::from_secs(60));
        engine.recover().await;
        let claim = engine
            .ops
            .capture_claim()
            .expect("the marker's frame holds the slot");
        assert!(claim.recovered);
        // The file existing is not the frame's end: PHD2 creates it
        // before it has written it.
        std::fs::write(&path, b"frame").unwrap();
        assert!(
            !engine
                .ops
                .wait_capture_released(&path, Duration::from_millis(200))
                .await,
            "the slot was released on the file alone"
        );
        engine
            .on_late_report(&FrameReport {
                success: true,
                error: None,
                path: Some(path.to_str().unwrap().to_string()),
            })
            .await;
        assert!(engine.ops.capture_claim().is_none());
        assert!(!path.exists());
        assert!(!engine.marker_path().exists());
    }

    #[tokio::test(start_paused = true)]
    async fn a_live_marker_holds_the_slot_no_longer_than_its_deadline() {
        let dir = tempfile::tempdir().unwrap();
        let engine = engine(dir.path());
        let path = dir.path().join(FRAME);
        stage_marker(&engine, &path, SystemTime::now() + Duration::from_secs(60));
        engine.recover().await;
        assert!(engine.ops.capture_claim().is_some());
        assert!(
            engine
                .ops
                .wait_capture_released(&path, Duration::from_secs(120))
                .await,
            "the slot outlived the marker's deadline"
        );
        assert!(!engine.marker_path().exists());
    }

    #[tokio::test]
    async fn a_marker_deadline_beyond_any_frame_is_cut_to_the_longest_one() {
        let dir = tempfile::tempdir().unwrap();
        let engine = engine(dir.path());
        let path = dir.path().join(FRAME);
        stage_marker(&engine, &path, SystemTime::now() + Duration::from_hours(24));
        engine.recover().await;
        let claim = engine
            .ops
            .capture_claim()
            .expect("the marker holds the slot");
        let longest = SystemTime::now() + MAX_EXPOSURE + MAX_CAPTURE_GRACE;
        assert!(claim.deadline <= longest, "{:?}", claim.deadline);
        // Written back: a restart loop cannot extend it from each start.
        let marker: Marker =
            serde_json::from_slice(&std::fs::read(engine.marker_path()).unwrap()).unwrap();
        assert_eq!(marker.deadline_unix_ms, unix_ms(claim.deadline));
        assert_eq!(marker.path, path);
    }

    #[tokio::test]
    async fn startup_without_a_marker_only_sweeps() {
        let dir = tempfile::tempdir().unwrap();
        let engine = engine(dir.path());
        let orphan = dir.path().join(FRAME);
        std::fs::write(&orphan, b"orphan").unwrap();
        engine.recover().await;
        assert!(engine.ops.capture_claim().is_none());
        assert!(!orphan.exists());
    }

    #[tokio::test]
    async fn an_abort_or_shutdown_with_no_frame_of_its_own_does_nothing() {
        let dir = tempfile::tempdir().unwrap();
        let engine = engine(dir.path());
        let path = dir.path().join(FRAME);
        assert!(engine.abort(&path, Duration::from_secs(1)).await);
        engine.shutdown().await;
        claim(&engine, &path, true).await;
        assert!(engine.abort(&path, Duration::from_secs(1)).await);
        engine.shutdown().await;
        assert!(
            engine.ops.capture_claim().is_some(),
            "a recovered frame was released"
        );
    }

    #[test]
    fn a_file_is_not_an_image_dir() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("plain");
        std::fs::write(&file, b"x").unwrap();
        let err = check_image_dir(&file).unwrap_err();
        assert!(err.contains("is not a directory"), "{err}");
    }

    #[test]
    fn discarding_a_missing_file_is_quiet() {
        let dir = tempfile::tempdir().unwrap();
        let engine = engine(dir.path());
        engine.discard(&dir.path().join(FRAME));
        remove_quietly(&dir.path().join("missing"));
    }

    #[test]
    fn the_engine_reports_its_grace() {
        let dir = tempfile::tempdir().unwrap();
        let engine = engine(dir.path());
        assert_eq!(engine.grace(), Duration::from_secs(10));
        assert_eq!(
            whole_ms(Duration::from_micros(1_500_400)),
            Duration::from_millis(1500)
        );
    }

    #[test]
    fn a_frame_name_is_the_prefix_32_hex_digits_and_fits() {
        assert!(is_frame_name(
            "phd2-guider-0123456789abcdef0123456789abcdef.fits"
        ));
        assert!(!is_frame_name("phd2-guider-capture.json"));
        assert!(!is_frame_name("phd2-guider-0123.fits"));
        assert!(!is_frame_name("save_image_abc123"));
        assert!(!is_frame_name(
            "phd2-guider-0123456789abcdef0123456789abcdeg.fits"
        ));
        // `start` never writes uppercase, so the sweep leaves such a file.
        assert!(!is_frame_name(
            "phd2-guider-0123456789ABCDEF0123456789ABCDEF.fits"
        ));
    }

    #[test]
    fn the_size_cap_is_the_data_in_whole_blocks_plus_eight_header_blocks() {
        // 752 x 580 x 2 = 872 320 bytes = 302.9 blocks, so 303 + 8.
        assert_eq!(frame_size_cap(752, 580), 311 * 2880);
        assert_eq!(frame_size_cap(8, 6), 9 * 2880);
    }

    #[test]
    fn the_sweep_removes_only_facade_frames_and_keeps_the_one_named() {
        let dir = tempfile::tempdir().unwrap();
        let orphan = dir
            .path()
            .join("phd2-guider-0123456789abcdef0123456789abcdef.fits");
        let kept = dir
            .path()
            .join("phd2-guider-ffffffffffffffffffffffffffffffff.fits");
        let foreign = dir.path().join("save_image_abc123");
        let marker = dir.path().join(MARKER_NAME);
        for path in [&orphan, &kept, &foreign, &marker] {
            std::fs::write(path, b"x").unwrap();
        }
        assert_eq!(sweep(dir.path(), Some(&kept)), 1);
        assert!(!orphan.exists());
        assert!(kept.exists());
        assert!(foreign.exists());
        assert!(marker.exists());
    }

    fn write_ramp(path: &Path, width: usize, height: usize) {
        let pixels: Vec<u16> = (0..width * height)
            .map(|i| u16::try_from((i * 1365) % 65536).unwrap())
            .collect();
        let mut file = std::fs::File::create(path).unwrap();
        rp_fits::writer::write_u16_image(&mut file, &pixels, width, height, &[]).unwrap();
    }

    #[test]
    fn a_frame_is_read_as_unsigned_values() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("f.fits");
        write_ramp(&path, 8, 6);
        let frame = read_frame(&path, 8, 6).unwrap();
        assert_eq!((frame.width, frame.height), (8, 6));
        assert_eq!(frame.pixels[0], 0);
        assert_eq!(frame.pixels[1], 1365);
        // Past the signed 16-bit midpoint, as PHD2's BZERO stores it.
        assert_eq!(frame.pixels[47], 47 * 1365);
    }

    /// A 2 x 1 frame: `cards` (its `BITPIX` and any scaling) between the
    /// size cards and `END`, then `data`, the two values big-endian.
    fn write_pair(path: &Path, cards: &[&str], data: &[u8]) {
        let header = [
            "SIMPLE  =                    T",
            "NAXIS   =                    2",
            "NAXIS1  =                    2",
            "NAXIS2  =                    1",
        ]
        .iter()
        .chain(cards)
        .chain(&["END"])
        .fold(String::new(), |mut header, card| {
            use std::fmt::Write as _;
            write!(header, "{card:<80}").unwrap();
            header
        });
        let mut bytes = format!("{header:<2880}").into_bytes();
        let mut data = data.to_vec();
        data.resize(2880, 0);
        bytes.extend_from_slice(&data);
        std::fs::write(path, bytes).unwrap();
    }

    const SIGNED_16: &str = "BITPIX  =                   16";

    /// A signed 16-bit 2 x 1 frame holding `values`, with `scaling` cards.
    fn write_signed_pair(path: &Path, scaling: &[&str], values: [i16; 2]) {
        let cards: Vec<&str> = std::iter::once(SIGNED_16)
            .chain(scaling.iter().copied())
            .collect();
        write_pair(
            path,
            &cards,
            &[values[0].to_be_bytes(), values[1].to_be_bytes()].concat(),
        );
    }

    #[test]
    fn a_frame_holding_a_value_below_zero_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("f.fits");
        write_signed_pair(&path, &[], [-1, 7]);
        let err = read_frame(&path, 2, 1).unwrap_err();
        assert!(err.contains("outside 0 to 65535"), "{err}");
    }

    #[test]
    fn a_frame_in_another_encoding_is_scaled_to_its_values() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("f.fits");
        write_signed_pair(&path, &[], [7, 32767]);
        assert_eq!(read_frame(&path, 2, 1).unwrap().pixels, [7, 32767]);
        write_signed_pair(
            &path,
            &[
                "BSCALE  =                    2",
                "BZERO   =                   10",
            ],
            [7, 100],
        );
        assert_eq!(read_frame(&path, 2, 1).unwrap().pixels, [24, 210]);
    }

    #[test]
    fn a_frame_scaled_by_a_fraction_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("f.fits");
        write_signed_pair(&path, &["BSCALE  =                  0.5"], [14, 15]);
        let err = read_frame(&path, 2, 1).unwrap_err();
        assert!(err.contains("BSCALE 0.5"), "{err}");
        assert!(err.contains("not both whole numbers"), "{err}");
    }

    #[test]
    fn a_floating_point_frame_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("f.fits");
        write_pair(
            &path,
            &["BITPIX  =                  -32"],
            &[7.0_f32.to_be_bytes(), 8.0_f32.to_be_bytes()].concat(),
        );
        let err = read_frame(&path, 2, 1).unwrap_err();
        assert!(err.contains("floating-point values"), "{err}");
    }

    #[test]
    fn a_frame_of_another_size_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("f.fits");
        write_ramp(&path, 8, 6);
        let err = read_frame(&path, 6, 8).unwrap_err();
        assert!(err.contains("frame size changed"), "{err}");
    }

    #[test]
    fn an_oversized_file_is_refused_unread() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("f.fits");
        std::fs::write(&path, vec![b' '; 10 * 2880]).unwrap();
        let err = read_frame(&path, 8, 6).unwrap_err();
        assert!(err.contains("more than a 8 x 6 frame needs"), "{err}");
    }

    #[cfg(unix)]
    #[test]
    fn a_link_at_the_frame_name_is_not_followed() {
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("target.fits");
        write_ramp(&target, 8, 6);
        let link = dir.path().join("f.fits");
        std::os::unix::fs::symlink(&target, &link).unwrap();
        let err = read_frame(&link, 8, 6).unwrap_err();
        assert!(err.contains("cannot open"), "{err}");
    }

    #[test]
    fn the_image_dir_check_names_the_directory_and_leaves_no_probe() {
        let dir = tempfile::tempdir().unwrap();
        check_image_dir(dir.path()).unwrap();
        assert!(!dir.path().join(PROBE_NAME).exists());
        let missing = dir.path().join("missing");
        let err = check_image_dir(&missing).unwrap_err();
        assert!(err.contains("missing"), "{err}");
    }

    #[test]
    fn a_capture_refusal_while_capturing_is_busy() {
        let err = map_rpc_error(Phd2Error::RpcError {
            code: 1,
            message: "cannot capture single frame when capture is currently active".into(),
        });
        assert!(matches!(err, StartError::Phd2Busy(_)), "{err:?}");
        let err = map_rpc_error(Phd2Error::RpcError {
            code: -32602,
            message: "destination file already exists".into(),
        });
        assert!(matches!(err, StartError::Phd2Refused(_)), "{err:?}");
        let err = map_rpc_error(Phd2Error::NotConnected);
        assert!(matches!(err, StartError::Unreachable(_)), "{err:?}");
    }
}
