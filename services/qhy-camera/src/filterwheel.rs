//! `QhyFilterWheelDevice` — the ASCOM `Device` + `FilterWheel` implementation
//! over the [`FilterWheelHandle`](crate::backend::FilterWheelHandle) seam.
//!
//! Registered automatically, one per discovered CFW (detection is the source of
//! truth — there is no opt-in toggle). `Names`
//! are the configured `filter_names` or generated `Filter0..N`; `Position`
//! returns `None` while the commanded target differs from the actual slot (ASCOM
//! "moving" sentinel), and an error once a move has outlived its deadline;
//! `FocusOffsets` is zero per filter in v0.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use ascom_alpaca::api::{Device, FilterWheel};
use ascom_alpaca::{ASCOMError, ASCOMResult};
use parking_lot::Mutex;
use qhyccd_rs::CfwStatus;
use tracing::{debug, warn};

use crate::backend::{BackendError, FilterWheelHandle, Verdict};
use crate::camera::UNSPECIFIED_ERROR;

/// The least time between the wheel's last status read returning and the next
/// move it is sent (FW5). A QHY CFW drops a move commanded within ~10 ms of the
/// read that saw the previous move arrive: the wheel never starts, and its
/// status goes on naming the slot it is at. 15 ms was already enough on a
/// QHY178M + CFW3; this is the margin, and a move takes seconds anyway.
const REST_AFTER_STATUS_READ: Duration = Duration::from_millis(250);

/// How many status reads a prime waits for the wheel to name the slot it was
/// sent back to (FW7). The first read named it every time it was measured; a
/// read of a wheel at rest takes ~255 ms, so this gives up after about 2 s.
const PRIME_CONFIRM_READS: u32 = 8;

/// How long a move has, from when its slot was sent, to arrive before it has
/// failed (FW8). The longest move measured, four slots on a seven-slot CFW3,
/// took 5.1 s; a full turn of a sixteen-slot wheel at its ~1.2 s a slot would
/// take ~19 s.
const MOVE_DEADLINE: Duration = Duration::from_secs(30);

/// The slot a status read names on a wheel of `count` slots: none while the
/// wheel reports itself moving (FW7), and none for a slot outside the count,
/// which is what the status decode makes of any other nonstandard byte.
fn named_slot(status: CfwStatus, count: usize) -> Option<usize> {
    match status {
        CfwStatus::Slot(slot) => usize::try_from(slot).ok().filter(|slot| *slot < count),
        CfwStatus::Moving => None,
    }
}

/// Slots are `usize` throughout because that is what every consumer is: the
/// ASCOM `Position`, and the `Names` / `FocusOffsets` lengths that must match
/// it. The SDK speaks `u32`, so the conversion sits at that seam — the
/// handshake below, and the two calls that read or command a slot.
#[derive(Debug)]
struct FilterWheelState {
    number_of_filters: Mutex<Option<usize>>,
    target_position: Mutex<Option<usize>>,
    /// Last slot read back from the SDK. Seeded at connect and refreshed only
    /// while a move is in flight, so a settled `Position` costs no SDK call —
    /// see [`QhyFilterWheelDevice::position`].
    settled_position: Mutex<Option<usize>>,
    /// When the wheel's last status read returned. Every status read and every
    /// move holds this lock across its SDK call, so a move can neither go out
    /// beside a read still in flight nor follow a finished one by less than
    /// [`REST_AFTER_STATUS_READ`] (FW5).
    last_status_read: Mutex<Option<Instant>>,
    /// The slot this connection last sent the wheel, `None` until it has sent
    /// one (FW7). Until it has, the Linux SDK may name a move's own target
    /// while the wheel travels; once it has, the slot named in transit is this
    /// one, so a move sent to it again — only ever a failed move's slot (FW2,
    /// FW8) — could read as arrived at once too. Cleared at every connect, and
    /// whenever what the SDK was last sent is not known: before a prime, and by
    /// a send that fails. Every write holds this lock for its whole sequence,
    /// so writes go out one at a time and a prime goes out with the move it
    /// serves.
    last_sent: Mutex<Option<u32>>,
    /// When the move under way was sent: from the send of its slot until a
    /// status read names that slot or the move fails (FW2, FW8).
    move_sent_at: Mutex<Option<Instant>>,
    /// What `Position` answers once a move has failed, until the next write or
    /// connect (FW8).
    failed_move: Mutex<Option<String>>,
    /// The wheel has reported itself moving (`'N'`, FW7), so its status never
    /// names a slot in transit and no move needs a prime. That is the SDK
    /// build's doing, so it holds for as long as the service runs.
    reports_moving: AtomicBool,
}

impl FilterWheelState {
    /// Read what the wheel's status reports, noting when the read returned
    /// (FW5) and whether it said the wheel is moving (FW7).
    fn read_status(&self, h: &dyn FilterWheelHandle) -> Result<CfwStatus, BackendError> {
        let mut last_read = self.last_status_read.lock();
        let status = h.get_position();
        *last_read = Some(Instant::now());
        drop(last_read);
        if matches!(status, Ok(CfwStatus::Moving)) {
            self.reports_moving.store(true, Ordering::SeqCst);
        }
        status
    }

    /// What a status read says of the wheel (FW1, FW8): the slot it rests on,
    /// `None` while it is still on its way, or the failure of a move that has
    /// had `deadline` to arrive and has not.
    fn take_reading(
        &self,
        status: CfwStatus,
        count: usize,
        deadline: Duration,
    ) -> Result<Option<usize>, String> {
        // One reading at a time, each against the target as the last left it,
        // so a failure one reading records is reported by the next rather than
        // undone by it.
        let mut target = self.target_position.lock();
        let failed = self.failed_move.lock().clone();
        if let Some(failure) = failed {
            return Err(failure);
        }
        let actual = named_slot(status, count);
        let out_of_time = self
            .move_sent_at
            .lock()
            .is_some_and(|at| at.elapsed() >= deadline);
        let reading = match (actual, *target) {
            // Reached the commanded slot.
            (Some(actual), Some(commanded)) if actual == commanded => {
                *self.settled_position.lock() = Some(actual);
                *self.move_sent_at.lock() = None;
                Ok(Some(actual))
            }
            // Connect could not read a slot, so there is nothing commanded to
            // reach — adopt the first real one the wheel reports.
            (Some(actual), None) => {
                *target = Some(actual);
                *self.settled_position.lock() = Some(actual);
                Ok(Some(actual))
            }
            // Not there, and out of time.
            (_, Some(commanded)) if out_of_time => {
                let names =
                    actual.map_or_else(|| "no slot".to_string(), |slot| format!("slot {slot}"));
                let failure = format!(
                    "the filter wheel did not reach slot {commanded} within {deadline:?}; its status names {names}"
                );
                warn!(%failure, "filter wheel move failed");
                // Where the wheel stopped is not known: under Linux the status
                // of a wheel that has not stopped names the slot sent before
                // the move, not the one it is on.
                *target = None;
                *self.move_sent_at.lock() = None;
                *self.settled_position.lock() = None;
                *self.failed_move.lock() = Some(failure.clone());
                Err(failure)
            }
            // Still travelling, or still not naming a slot.
            _ => Ok(None),
        };
        drop(target);
        reading
    }

    /// A client's write of `slot` (`target` to the SDK) to a wheel of `count`
    /// slots (FW2, FW5, FW7, FW8). A move under way whose deadline has passed
    /// is settled first, by a status read, so the write is not refused on
    /// behalf of a move that has already failed.
    fn write(
        &self,
        h: &dyn FilterWheelHandle,
        slot: usize,
        target: u32,
        count: usize,
        deadline: Duration,
    ) -> Result<Write, BackendError> {
        let mut last_sent = self.last_sent.lock();
        if self
            .move_sent_at
            .lock()
            .is_some_and(|at| at.elapsed() >= deadline)
        {
            let status = self.read_status(h)?;
            if self.take_reading(status, count, deadline).is_err() {
                debug!(
                    slot,
                    "the move before this write failed; the write goes out all the same"
                );
            }
        }
        let commanded = *self.target_position.lock();
        if commanded == Some(slot) {
            return Ok(Write::AlreadyCommanded);
        }
        if let Some(under_way) = commanded.filter(|_| self.move_sent_at.lock().is_some()) {
            return Ok(Write::Refused { under_way });
        }
        // The client has acted on a failed move: from here `Position` reads
        // the wheel again, whatever becomes of this write (FW8).
        *self.failed_move.lock() = None;
        if !self.reports_moving.load(Ordering::SeqCst)
            && last_sent.is_none_or(|last| last == target)
        {
            // No move of this connection's is under way, so the wheel stands on
            // the slot last read. Where none is held, as after a failed move,
            // the status is read afresh; a wheel it names no slot for has no
            // slot to go back to.
            let held = *self.settled_position.lock();
            let stands_at = match held {
                Some(slot) => Some(slot),
                None => named_slot(self.read_status(h)?, count),
            }
            .and_then(|slot| u32::try_from(slot).ok());
            if let Some(stands_at) = stands_at {
                *last_sent = None;
                self.prime(h, stands_at)?;
            }
        }
        let sent = self.command_slot(h, target);
        if sent.is_ok() {
            *self.target_position.lock() = Some(slot);
            *self.move_sent_at.lock() = Some(Instant::now());
        }
        *last_sent = sent.is_ok().then_some(target);
        drop(last_sent);
        sent.map(|()| Write::Sent)
    }

    /// Send the wheel to the slot it stands on, which does not move it, and
    /// read until the status names that slot. A move sent next then names this
    /// slot in transit, not its own target (FW7).
    fn prime(&self, h: &dyn FilterWheelHandle, stands_at: u32) -> Result<(), BackendError> {
        debug!(
            slot = stands_at,
            "sending the wheel to the slot it stands on first, so its status cannot name the move's own slot in transit"
        );
        self.command_slot(h, stands_at)?;
        for _ in 0..PRIME_CONFIRM_READS {
            if self.read_status(h)? == CfwStatus::Slot(stands_at) {
                return Ok(());
            }
        }
        Err(BackendError(format!(
            "the wheel's status did not name slot {stands_at}, the one it stands on"
        )))
    }

    /// Send the wheel to `slot`, no sooner than [`REST_AFTER_STATUS_READ`]
    /// after its last status read returned (FW5).
    fn command_slot(&self, h: &dyn FilterWheelHandle, slot: u32) -> Result<(), BackendError> {
        let last_read = self.last_status_read.lock();
        let rest = last_read.map_or(Duration::ZERO, |at| {
            REST_AFTER_STATUS_READ.saturating_sub(at.elapsed())
        });
        if !rest.is_zero() {
            debug!(
                ?rest,
                slot, "the wheel's status was just read; resting before the move"
            );
            std::thread::sleep(rest);
        }
        let sent = h.set_position(slot);
        drop(last_read);
        sent
    }
}

/// What became of a client's write of a slot (FW2).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Write {
    /// The slot went out to the wheel.
    Sent,
    /// The slot was the one already commanded, under way or reached.
    AlreadyCommanded,
    /// A move to another slot is under way, and a CFW drops a move sent while
    /// it travels.
    Refused { under_way: usize },
}

/// One ASCOM `FilterWheel` device per discovered CFW.
#[derive(Clone, derive_more::Debug)]
pub struct QhyFilterWheelDevice {
    #[debug(skip)]
    handle: Arc<dyn FilterWheelHandle>,
    unique_id: String,
    name: String,
    /// Human filter names from config (overrides generated `Filter0..N`).
    filter_names: Option<Vec<String>>,
    state: Arc<FilterWheelState>,
    /// How long a move has to arrive ([`MOVE_DEADLINE`]).
    move_deadline: Duration,
}

impl QhyFilterWheelDevice {
    /// Build a CFW device. The ASCOM `UniqueID` is `CFW-<sdk-id>` (prefixed so it
    /// never collides with the camera's `UniqueID`, which shares the SDK id on
    /// single-handle models). `filter_names` / `name` come from the per-serial
    /// config override.
    pub fn new(
        handle: Arc<dyn FilterWheelHandle>,
        filter_names: Option<Vec<String>>,
        name: Option<String>,
    ) -> Self {
        let id = handle.id();
        let unique_id = format!("CFW-{id}");
        let name = name.unwrap_or_else(|| format!("QHYCCD Filter Wheel {id}"));
        Self {
            handle,
            unique_id,
            name,
            filter_names,
            state: Arc::new(FilterWheelState {
                number_of_filters: Mutex::new(None),
                target_position: Mutex::new(None),
                settled_position: Mutex::new(None),
                last_status_read: Mutex::new(None),
                last_sent: Mutex::new(None),
                move_sent_at: Mutex::new(None),
                failed_move: Mutex::new(None),
                reports_moving: AtomicBool::new(false),
            }),
            move_deadline: MOVE_DEADLINE,
        }
    }

    /// Shorten the deadline a move has to arrive, so a test can reach a failed
    /// move without waiting out [`MOVE_DEADLINE`].
    #[cfg(test)]
    const fn with_move_deadline(mut self, deadline: Duration) -> Self {
        self.move_deadline = deadline;
        self
    }

    fn ensure_connected(&self) -> ASCOMResult<()> {
        if self.is_connected() {
            Ok(())
        } else {
            Err(ASCOMError::NOT_CONNECTED)
        }
    }

    /// Whether the wheel holds a session on a camera still on the bus: the
    /// handle is open and the shared connection has not been marked lost (C9,
    /// FW4). A handle whose `is_open` fails counts as closed.
    fn is_connected(&self) -> bool {
        self.handle.is_open().unwrap_or_else(|e| {
            debug!(filter_wheel = %self.unique_id, error = %e, "is_open() failed; reporting disconnected");
            false
        }) && !self.handle.is_lost()
    }

    fn filter_count(&self) -> ASCOMResult<usize> {
        (*self.state.number_of_filters.lock()).ok_or(ASCOMError::NOT_CONNECTED)
    }

    /// Run one SDK-touching step off the async executor, as
    /// [`QhyCameraDevice::on_handle`](crate::camera::QhyCameraDevice) does. A CFW
    /// status query is a serial round-trip *through the camera* — ~260 ms on a
    /// QHY178M + CFW3, the slowest single SDK call in this service — so making
    /// one on a Tokio worker stalls every request sharing it for a quarter of a
    /// second.
    ///
    /// A request that lost a race with a disconnect reports `NOT_CONNECTED`
    /// whatever the SDK said, for the same reason as the camera's `on_handle`:
    /// [`Self::ensure_connected`] runs before the hop and is a check, not a
    /// guard, so a slot read that lands after the close would otherwise report
    /// `INVALID_OPERATION` purely because of where in the race it fell.
    ///
    /// A failure also asks whether the camera is still on the bus, as the
    /// camera's does (C9, FW4), so a wheel whose camera has gone answers
    /// `NOT_CONNECTED` too.
    async fn on_handle<T, F>(&self, f: F) -> ASCOMResult<T>
    where
        F: FnOnce(&dyn FilterWheelHandle) -> ASCOMResult<T> + Send + 'static,
        T: Send + 'static,
    {
        let handle = Arc::clone(&self.handle);
        let (outcome, gone) = tokio::task::spawn_blocking(move || {
            let generation = handle.generation();
            let outcome = f(handle.as_ref());
            // The verdict is this request's answer. Read again once the task
            // is back, the connection may already belong to a session another
            // client's release and reconnect opened since. A reconnect that
            // landed before the question was put means the handle this call
            // failed on is gone too, whatever the fresh one answers.
            let gone = outcome.is_err()
                && (handle.verify_presence() == Verdict::Lost || handle.generation() != generation);
            (outcome, gone)
        })
        .await
        .map_err(|e| ASCOMError::invalid_operation(format!("SDK task failed: {e}")))?;
        match outcome {
            Err(e) if gone || self.ensure_connected().is_err() => {
                debug!(error = %e, "SDK call failed on a handle that is closed or whose camera has left the bus");
                Err(ASCOMError::NOT_CONNECTED)
            }
            outcome => outcome,
        }
    }

    /// The open + handshake, off the executor: the handshake reads the slot
    /// count and the current slot, both SDK round-trips.
    async fn connect(&self) -> ASCOMResult<()> {
        let device = self.clone();
        tokio::task::spawn_blocking(move || device.connect_blocking())
            .await
            .map_err(|e| ASCOMError::invalid_operation(format!("connect task failed: {e}")))?
    }

    /// Await a spawned section that owns the physical connection, in a way a
    /// cancelled request cannot cut short — the wheel's copy of the camera's
    /// [`QhyCameraDevice::detached`](crate::camera) rule.
    ///
    /// Dropping a `JoinHandle` detaches its task rather than stopping it, so the
    /// section runs to completion — and gives the connection back — even when the
    /// request that started it goes away.
    async fn detached<T>(task: tokio::task::JoinHandle<ASCOMResult<T>>) -> ASCOMResult<T> {
        task.await
            .map_err(|e| ASCOMError::invalid_operation(format!("device task failed: {e}")))?
    }

    fn connect_blocking(&self) -> ASCOMResult<()> {
        // `handle.open()` is refcounted across the shared physical connection
        // (`backend::SharedCameraConnection`): a QHY CFW is driven through the
        // camera's USB handle, so the Camera and FilterWheel devices on the same
        // SDK id share ONE `OpenQHYCCD`. Opening the wheel just bumps that
        // refcount (physically opening only if it is the first connect).
        self.handle.open().map_err(|_| ASCOMError::NOT_CONNECTED)?;
        // If any step of the post-open handshake fails, close the handle (drop our
        // refcount) before propagating so a failed connect leaves Connected ==
        // false rather than an opened-but-unusable wheel (mirrors the camera).
        if let Err(e) = self.open_handshake() {
            if let Err(close_err) = self.handle.close() {
                debug!(error = %close_err, "close after a failed filter-wheel connect handshake also failed");
            }
            return Err(e);
        }
        Ok(())
    }

    fn open_handshake(&self) -> ASCOMResult<()> {
        let count = self
            .handle
            .get_number_of_filters()
            .map_err(|_| ASCOMError::NOT_CONNECTED)?;
        // Initial target = the current physical slot. This is also the one place
        // an idle wheel reads the SDK: from here on the slot only changes when
        // this driver commands it, so `position` serves the settled value from
        // cache (FW1).
        let status = self
            .state
            .read_status(self.handle.as_ref())
            .map_err(|_| ASCOMError::NOT_CONNECTED)?;
        // The slot count sizes `Names` and `FocusOffsets`, so a wheel reporting
        // one this target cannot address has not handshaken.
        let Ok(count) = usize::try_from(count) else {
            return Err(ASCOMError::NOT_CONNECTED);
        };
        // The status is different: a wheel that reports itself moving, or
        // names no slot it has, is still on its way somewhere. ASCOM's answer
        // for that is the moving sentinel (`Position` = -1), not a refused
        // connect, so cache no slot and let `position` adopt one as soon as the
        // wheel reports a real one.
        let settled = named_slot(status, count);
        if settled.is_none() {
            debug!(
                filter_wheel = %self.unique_id,
                slots = count,
                ?status,
                "CFW reported no slot at connect; Position stays the moving sentinel until it does"
            );
        }
        *self.state.number_of_filters.lock() = Some(count);
        *self.state.target_position.lock() = settled;
        *self.state.settled_position.lock() = settled;
        // The SDK keeps the slot it was last sent across a close and re-open,
        // and this connection does not know which that is (FW7). A move the
        // last connection sent, or saw fail, is not this one's (FW8).
        *self.state.last_sent.lock() = None;
        *self.state.move_sent_at.lock() = None;
        *self.state.failed_move.lock() = None;
        debug!(filter_wheel = %self.unique_id, slots = count, "filter wheel connected");
        Ok(())
    }

    async fn disconnect(&self) -> ASCOMResult<()> {
        // Refcounted close (`backend::SharedCameraConnection`): the underlying
        // camera is physically closed only when the LAST device sharing this SDK
        // id disconnects. Disconnecting the wheel therefore no longer tears down a
        // concurrently-connected camera — the real-hardware failure mode flagged
        // in review and confirmed before this fix. See docs/services/qhy-camera.md.
        self.on_handle(|h| h.close().map_err(|_| ASCOMError::NOT_CONNECTED))
            .await
    }

    /// End a session whose camera has left the bus, judged by whether the wheel
    /// let go of the connection rather than by what `CloseQHYCCD` said about a
    /// device that is no longer there — the camera's rule (C9, FW4).
    async fn release_lost(&self) -> ASCOMResult<()> {
        match self.disconnect().await {
            Err(e) if !self.handle.is_open().unwrap_or(true) => {
                debug!(filter_wheel = %self.unique_id, error = %e, "close of a wheel whose camera has left the bus failed; its session is released regardless");
                Ok(())
            }
            released => released,
        }
    }
}

#[async_trait::async_trait]
impl Device for QhyFilterWheelDevice {
    fn static_name(&self) -> &str {
        &self.name
    }

    fn unique_id(&self) -> &str {
        &self.unique_id
    }

    async fn connected(&self) -> ASCOMResult<bool> {
        // A `Connected` GET must be a safe boolean so health/management polling
        // never throws (matches every sibling driver). Report `false` if the seam
        // ever fails rather than erroring. `is_open()` is infallible in every
        // current backend (it reads an atomic), so the fallback is purely
        // defensive — the *mutating* `set_connected` below intentionally still
        // propagates the error, since a misread there would drive a wrong
        // open/close. A wheel whose camera has left the bus reads false while
        // its handle is still open (C9, FW4).
        Ok(self.is_connected())
    }

    async fn set_connected(&self, connected: bool) -> ASCOMResult<()> {
        // Spawned rather than run in this request future, for the reason the
        // camera's is (see [`Self::detached`]): `connect` hands its handshake to
        // `spawn_blocking`, which a dropped `JoinHandle` detaches rather than
        // stops, so a guard held out here would be released by a cancelled
        // request while the SDK calls it was ordering carried on.
        let device = self.clone();
        Self::detached(tokio::spawn(async move {
            // Taken before the state is read (C8), for the reason the camera's
            // is — and off the handle, so it is the same lock the Camera device
            // on this physical connection takes rather than one of the wheel's
            // own.
            let _lifecycle = device.handle.lifecycle_lock().lock().await;
            let held = device
                .handle
                .is_open()
                .map_err(|_| ASCOMError::NOT_CONNECTED)?;
            // Held but not connected once the camera has left the bus, and
            // released before anything else either way — the camera's rule (C9).
            let lost = held && device.handle.is_lost();
            if connected == held && !lost {
                return Ok(());
            }
            if lost {
                device.release_lost().await?;
            }
            if connected {
                device.connect().await
            } else if lost {
                Ok(())
            } else {
                device.disconnect().await
            }
        }))
        .await
    }

    async fn description(&self) -> ASCOMResult<String> {
        Ok("QHYCCD filter wheel".to_string())
    }

    async fn driver_info(&self) -> ASCOMResult<String> {
        Ok("rusty-photon qhy-camera".to_string())
    }

    async fn driver_version(&self) -> ASCOMResult<String> {
        Ok(env!("CARGO_PKG_VERSION").to_string())
    }
}

#[async_trait::async_trait]
impl FilterWheel for QhyFilterWheelDevice {
    async fn names(&self) -> ASCOMResult<Vec<String>> {
        self.ensure_connected()?;
        let count = self.filter_count()?;
        // ASCOM requires the `Names` array to have exactly one entry per slot
        // (matching `FocusOffsets` and the `Position` range). The hardware slot
        // count is unknown until connect, so configured `filter_names` cannot be
        // validated at config-load time — normalise here: take the first `count`
        // configured names and pad any remainder with generated `Filter{i}`.
        Ok((0..count)
            .map(|i| {
                self.filter_names
                    .as_ref()
                    .and_then(|names| names.get(i).cloned())
                    .unwrap_or_else(|| format!("Filter{i}"))
            })
            .collect())
    }

    async fn focus_offsets(&self) -> ASCOMResult<Vec<i32>> {
        self.ensure_connected()?;
        let count = self.filter_count()?;
        Ok(vec![0; count])
    }

    async fn position(&self) -> ASCOMResult<Option<usize>> {
        self.ensure_connected()?;
        let count = self.filter_count()?;
        // A failed move is reported until the next write (FW8).
        let failed = self.state.failed_move.lock().clone();
        if let Some(failure) = failed {
            return Err(ASCOMError::new(UNSPECIFIED_ERROR, failure));
        }
        let target = *self.state.target_position.lock();

        // A settled wheel answers from cache. The SDK's CFW status query is a
        // serial round-trip through the camera (~260 ms on a QHY178M + CFW3),
        // which alone puts `Position` outside ASCOM's 100 ms target for a state
        // getter — and nothing moves the wheel except `set_position` below, so
        // there is nothing to re-read until a move is outstanding. INDI's
        // `indi-qhy` takes the same approach: `QueryFilter()` returns a cached
        // member and `GetQHYCCDCFWStatus` runs only while a move is in flight.
        if target.is_some() && *self.state.settled_position.lock() == target {
            return Ok(target);
        }

        let state = Arc::clone(&self.state);
        let status = self
            .on_handle(move |h| {
                state
                    .read_status(h)
                    .map_err(|_| ASCOMError::INVALID_OPERATION)
            })
            .await?;
        // `None` is the ASCOM "moving" sentinel (`Position` = -1).
        self.state
            .take_reading(status, count, self.move_deadline)
            .map_err(|failure| ASCOMError::new(UNSPECIFIED_ERROR, failure))
    }

    async fn set_position(&self, position: usize) -> ASCOMResult<()> {
        self.ensure_connected()?;
        let count = self.filter_count()?;
        // Range-check the slot as ASCOM sent it. Narrowing to the SDK's `u32`
        // first would have wrapped `2^32` onto slot 0 and passed this check.
        if position >= count {
            return Err(ASCOMError::invalid_value(format!(
                "filter position {position} out of range (0..{count})"
            )));
        }
        // `position < count`, and the count itself came from an SDK `u32`.
        let target = u32::try_from(position).map_err(|_| ASCOMError::INVALID_OPERATION)?;
        let state = Arc::clone(&self.state);
        let deadline = self.move_deadline;
        let write = self
            .on_handle(move |h| {
                state
                    .write(h, position, target, count, deadline)
                    .map_err(|_| ASCOMError::INVALID_OPERATION)
            })
            .await?;
        match write {
            // The slot already commanded is not sent again, settled or not. In
            // transit the Linux SDK's status names the slot commanded before the
            // move, so a resend of a dropped move names its own slot from the
            // first read and would read as an arrival while the wheel still
            // turns (FW5).
            Write::Sent | Write::AlreadyCommanded => Ok(()),
            // Sent, it would be dropped, stranding the wheel (FW2, FW5).
            Write::Refused { under_way } => Err(ASCOMError::invalid_operation(format!(
                "the filter wheel is still moving to slot {under_way}"
            ))),
        }
    }
}

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
mod tests {
    use super::*;
    use crate::backend::mock::{drop_once_parked, MockFilterWheelHandle};
    use ascom_alpaca::ASCOMErrorCode;
    use std::sync::atomic::Ordering;

    /// C8 under cancellation: the wheel's transition is spawned too, so a
    /// cancelled request does not hand the shared connection on while its own
    /// handshake is still in the SDK.
    #[tokio::test(flavor = "multi_thread")]
    async fn a_cancelled_connect_holds_the_connection_until_its_handshake_is_done() {
        let lifecycle = Arc::new(tokio::sync::Mutex::new(()));
        let handle = Arc::new(
            MockFilterWheelHandle::new("SIM-QHY178M", 7).with_lifecycle(Arc::clone(&lifecycle)),
        );
        let device =
            QhyFilterWheelDevice::new(Arc::<MockFilterWheelHandle>::clone(&handle), None, None);

        let hold = handle.hold_open_until_dropped();
        drop_once_parked(device.set_connected(true), || handle.is_in_open()).await;
        assert!(
            lifecycle.try_lock().is_err(),
            "a cancelled request must not give the connection back while the handshake it guards is still running"
        );

        // Wait for the handshake to publish, not for `Connected`: `open()` makes
        // the handle report open before the slot count behind it is cached, so
        // gating on `Connected` would race the assertions below.
        drop(hold);
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
        while device.names().await.is_err() {
            assert!(
                std::time::Instant::now() < deadline,
                "the detached connect never published"
            );
            tokio::time::sleep(std::time::Duration::from_millis(1)).await;
        }
        assert!(device.connected().await.unwrap());
        assert_eq!(handle.handshake_calls.load(Ordering::SeqCst), 1);
        assert_eq!(device.names().await.unwrap().len(), 7);
    }

    /// C8: the wheel is held to one connect at a time too — its handshake goes
    /// down the same physical `OpenQHYCCD` the camera's does, and a burst that
    /// each read a closed handle would each run one.
    #[tokio::test(flavor = "multi_thread")]
    async fn a_burst_of_connects_runs_one_handshake() {
        let handle = Arc::new(MockFilterWheelHandle::new("SIM-QHY178M", 7));
        let device =
            QhyFilterWheelDevice::new(Arc::<MockFilterWheelHandle>::clone(&handle), None, None);

        handle.hold_open();
        let connects = (0..4_u8)
            .map(|_| {
                let device = device.clone();
                tokio::spawn(async move { device.set_connected(true).await })
            })
            .collect::<Vec<_>>();
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
        while !handle.is_in_open() {
            assert!(
                std::time::Instant::now() < deadline,
                "the connect never reached the open"
            );
            tokio::time::sleep(std::time::Duration::from_millis(1)).await;
        }
        handle.release_open();
        for connect in connects {
            connect.await.unwrap().unwrap();
        }

        assert!(device.connected().await.unwrap());
        assert_eq!(
            handle.handshake_calls.load(Ordering::SeqCst),
            1,
            "the three behind the first found the wheel already where they wanted it"
        );
        assert_eq!(device.names().await.unwrap().len(), 7);
    }

    async fn connected(filter_names: Option<Vec<String>>) -> QhyFilterWheelDevice {
        let handle = Arc::new(MockFilterWheelHandle::new("SIM-QHY178M", 7));
        let device = QhyFilterWheelDevice::new(handle, filter_names, None);
        device.connect().await.unwrap();
        device
    }

    /// Same rule as the camera's `on_handle`: a slot read that lost a race with
    /// a disconnect reports the disconnect, not whichever code the call site
    /// spells a dead handle as.
    #[tokio::test]
    async fn a_call_that_loses_a_race_with_a_disconnect_reports_not_connected() {
        let device = connected(None).await;
        let err = device
            .on_handle(|h| {
                h.close().unwrap();
                Err::<(), _>(ASCOMError::INVALID_OPERATION)
            })
            .await
            .unwrap_err();
        assert_eq!(err.code, ASCOMErrorCode::NOT_CONNECTED);
    }

    #[tokio::test]
    async fn a_settled_position_is_served_without_an_sdk_read() {
        // The SDK's CFW status query is a serial round-trip through the camera
        // (~260 ms on a QHY178M + CFW3), which alone puts `Position` outside
        // ASCOM's 100 ms target for a state getter. Nothing moves the wheel but
        // this driver, so a settled read must not reach the SDK at all.
        let handle = Arc::new(MockFilterWheelHandle::new("SIM-QHY178M", 7));
        let device = QhyFilterWheelDevice::new(handle.clone(), None, None);
        device.connect().await.unwrap();

        let after_connect = handle.get_position_calls.load(Ordering::SeqCst);
        for _ in 0..5 {
            assert_eq!(device.position().await.unwrap(), Some(0));
        }
        assert_eq!(
            handle.get_position_calls.load(Ordering::SeqCst),
            after_connect
        );
    }

    #[tokio::test]
    async fn an_outstanding_move_polls_the_sdk_until_it_settles() {
        let handle = Arc::new(MockFilterWheelHandle::new("SIM-QHY178M", 7));
        handle.defer_move.store(true, Ordering::SeqCst);
        let device = QhyFilterWheelDevice::new(handle.clone(), None, None);
        device.connect().await.unwrap();

        device.set_position(3).await.unwrap();
        // In flight: the ASCOM moving sentinel, and the driver is reading the SDK.
        let before = handle.get_position_calls.load(Ordering::SeqCst);
        assert_eq!(device.position().await.unwrap(), None);
        assert!(handle.get_position_calls.load(Ordering::SeqCst) > before);

        handle.complete_move();
        assert_eq!(device.position().await.unwrap(), Some(3));

        // Settled again, so reads stop touching the SDK.
        let settled = handle.get_position_calls.load(Ordering::SeqCst);
        assert_eq!(device.position().await.unwrap(), Some(3));
        assert_eq!(handle.get_position_calls.load(Ordering::SeqCst), settled);
    }

    /// FW5: a client that commands the next slot as soon as `Position` names
    /// the last one gets its move sent only after the wheel has rested from
    /// the read that saw it arrive — sent straight after, a CFW drops it.
    #[tokio::test]
    async fn a_move_rests_after_the_read_that_saw_the_last_one_arrive() {
        let handle = Arc::new(MockFilterWheelHandle::new("SIM-QHY178M", 7));
        handle.defer_move.store(true, Ordering::SeqCst);
        let device = QhyFilterWheelDevice::new(handle.clone(), None, None);
        device.connect().await.unwrap();
        device.set_position(3).await.unwrap();
        handle.complete_move();
        assert_eq!(device.position().await.unwrap(), Some(3));

        device.set_position(5).await.unwrap();

        let arrival_read = *handle.reads_returned().last().unwrap();
        let next_move = *handle.moves_sent().last().unwrap();
        assert!(
            next_move.duration_since(arrival_read) >= REST_AFTER_STATUS_READ,
            "the move went out {:?} after the read that saw the wheel arrive",
            next_move.duration_since(arrival_read)
        );
    }

    /// FW5: the rest is what is left of it, not a delay on every move — a
    /// wheel that has been still for longer is sent its move at once.
    #[tokio::test]
    async fn a_move_to_a_rested_wheel_goes_out_at_once() {
        let handle = Arc::new(MockFilterWheelHandle::new("SIM-QHY178M", 7));
        let device = QhyFilterWheelDevice::new(handle.clone(), None, None);
        device.connect().await.unwrap();
        // Past the connection's first move, whose prime reads the status
        // straight before it (FW7).
        device.set_position(3).await.unwrap();
        assert_eq!(device.position().await.unwrap(), Some(3));
        tokio::time::sleep(REST_AFTER_STATUS_READ).await;

        let asked = std::time::Instant::now();
        device.set_position(5).await.unwrap();

        let sent = *handle.moves_sent().last().unwrap();
        assert!(
            sent.duration_since(asked) < REST_AFTER_STATUS_READ / 2,
            "a rested wheel's move waited {:?}",
            sent.duration_since(asked)
        );
    }

    /// FW5: a move does not go out beside a status read still in flight — a
    /// second client's `Position` poll — but waits for it, then rests. A
    /// wheel that named no slot at connect is polled with no move under way,
    /// so a write then is not refused (FW2).
    #[tokio::test(flavor = "multi_thread")]
    async fn a_move_waits_for_a_status_read_in_flight_and_then_rests() {
        let handle = Arc::new(MockFilterWheelHandle::new("SIM-QHY178M", 7));
        handle.set_reported_position(30);
        let device = QhyFilterWheelDevice::new(handle.clone(), None, None);
        device.connect().await.unwrap();
        let sent_before = handle.moves_sent().len();

        let hold = handle.hold_read_until_dropped();
        let poller = device.clone();
        let poll = tokio::spawn(async move { poller.position().await });
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
        while !handle.is_in_read() {
            assert!(
                std::time::Instant::now() < deadline,
                "the poll never reached the SDK"
            );
            tokio::time::sleep(std::time::Duration::from_millis(1)).await;
        }
        let mover = device.clone();
        let next = tokio::spawn(async move { mover.set_position(5).await });
        // Long enough for a move that did not wait for the read to have gone.
        tokio::time::sleep(REST_AFTER_STATUS_READ * 2).await;
        assert_eq!(
            handle.moves_sent().len(),
            sent_before,
            "a move was sent while a status read was still in flight"
        );

        drop(hold);
        assert_eq!(poll.await.unwrap().unwrap(), None);
        next.await.unwrap().unwrap();
        let read = *handle.reads_returned().last().unwrap();
        let next_move = *handle.moves_sent().last().unwrap();
        assert!(next_move.duration_since(read) >= REST_AFTER_STATUS_READ);
    }

    #[tokio::test]
    async fn failed_handshake_closes_the_handle() {
        // open() succeeds but the post-open handshake fails: a failed connect
        // must leave the wheel cleanly disconnected, not opened-but-unusable.
        let handle = Arc::new(MockFilterWheelHandle::new("SIM-QHY178M", 7));
        handle.fail_handshake.store(true, Ordering::SeqCst);
        let device = QhyFilterWheelDevice::new(handle.clone(), None, None);

        let err = device.connect().await.unwrap_err();
        assert_eq!(err.code, ASCOMErrorCode::NOT_CONNECTED);
        assert!(
            !handle.is_open().unwrap(),
            "handle must be closed after a failed connect handshake"
        );
    }

    #[tokio::test]
    async fn set_connected_toggles_and_is_idempotent() {
        // Drives `set_connected` (both branches) + `disconnect()` end to end —
        // the connect/disconnect lifecycle the other tests skip by calling
        // `connect()` directly.
        let handle = Arc::new(MockFilterWheelHandle::new("SIM-QHY178M", 7));
        let device = QhyFilterWheelDevice::new(handle, None, None);
        assert!(!device.connected().await.unwrap());

        // connect via set_connected (connect branch + handshake)
        device.set_connected(true).await.unwrap();
        assert!(device.connected().await.unwrap());
        assert_eq!(device.names().await.unwrap().len(), 7);

        // already connected → no-op (the current == connected early return)
        device.set_connected(true).await.unwrap();
        assert!(device.connected().await.unwrap());

        // disconnect via set_connected (disconnect branch)
        device.set_connected(false).await.unwrap();
        assert!(!device.connected().await.unwrap());
        // operations after disconnect report NOT_CONNECTED (ensure_connected)
        assert_eq!(
            device.names().await.unwrap_err().code,
            ASCOMErrorCode::NOT_CONNECTED
        );
        assert_eq!(
            device.position().await.unwrap_err().code,
            ASCOMErrorCode::NOT_CONNECTED
        );

        // already disconnected → no-op
        device.set_connected(false).await.unwrap();
        assert!(!device.connected().await.unwrap());
    }

    #[tokio::test]
    async fn generated_names_when_no_config() {
        let device = connected(None).await;
        let names = device.names().await.unwrap();
        assert_eq!(names.len(), 7);
        assert_eq!(names[0], "Filter0");
        assert_eq!(names[6], "Filter6");
    }

    #[tokio::test]
    async fn custom_names_from_config() {
        let custom = vec!["L", "R", "G", "B", "Ha", "OIII", "SII"]
            .into_iter()
            .map(String::from)
            .collect::<Vec<_>>();
        let device = connected(Some(custom.clone())).await;
        assert_eq!(device.names().await.unwrap(), custom);
    }

    #[tokio::test]
    async fn too_few_config_names_are_padded_to_slot_count() {
        let device = connected(Some(vec!["L".into(), "R".into(), "G".into()])).await;
        let names = device.names().await.unwrap();
        assert_eq!(names.len(), 7, "Names must have one entry per slot");
        assert_eq!(names[0], "L");
        assert_eq!(names[2], "G");
        assert_eq!(names[3], "Filter3");
        assert_eq!(names[6], "Filter6");
    }

    #[tokio::test]
    async fn too_many_config_names_are_truncated_to_slot_count() {
        let nine = (0..9).map(|i| format!("F{i}")).collect::<Vec<_>>();
        let device = connected(Some(nine)).await;
        let names = device.names().await.unwrap();
        assert_eq!(names.len(), 7, "Names must have one entry per slot");
        assert_eq!(names[0], "F0");
        assert_eq!(names[6], "F6");
    }

    #[tokio::test]
    async fn moving_to_a_valid_slot_updates_position() {
        let device = connected(None).await;
        device.set_position(3).await.unwrap();
        // The simulated CFW move settles over a few polls; poll until it reports
        // the target (`None` is the ASCOM "moving" sentinel).
        let mut pos = None;
        for _ in 0..10 {
            pos = device.position().await.unwrap();
            if pos == Some(3) {
                break;
            }
        }
        assert_eq!(pos, Some(3));
    }

    #[tokio::test]
    async fn out_of_range_slot_is_rejected() {
        let device = connected(None).await;
        assert_eq!(
            device.set_position(7).await.unwrap_err().code,
            ASCOMErrorCode::INVALID_VALUE
        );
        assert_eq!(
            device.set_position(99).await.unwrap_err().code,
            ASCOMErrorCode::INVALID_VALUE
        );
    }

    #[tokio::test]
    async fn a_wheel_naming_no_slot_at_connect_reports_moving_then_adopts_one() {
        // The status decode makes any nonstandard CFW status byte other than
        // 'N' into `byte - 0x30`, which for anything past 'F' lands outside the
        // wheel's slot count. That is a status which does not name a slot.
        // ASCOM's answer is the moving sentinel (`Position` = -1), not a
        // refused connect and not a slot `Names` has no entry for.
        let handle = Arc::new(MockFilterWheelHandle::new("SIM-QHY178M", 7));
        handle.set_reported_position(30);
        let device = QhyFilterWheelDevice::new(handle.clone(), None, None);

        device.connect().await.unwrap();
        assert_eq!(device.position().await.unwrap(), None);
        // `Names` is still sized from the slot count, which did read cleanly.
        assert_eq!(device.names().await.unwrap().len(), 7);

        // Once the wheel names a real slot, the driver adopts it...
        handle.set_reported_position(4);
        assert_eq!(device.position().await.unwrap(), Some(4));

        // ...and it is settled, so further reads stop touching the SDK.
        let settled = handle.get_position_calls.load(Ordering::SeqCst);
        assert_eq!(device.position().await.unwrap(), Some(4));
        assert_eq!(handle.get_position_calls.load(Ordering::SeqCst), settled);
    }

    /// A wheel already on its way somewhere when the client connects, on an
    /// SDK that passes the CFW's `'N'` through (FW7).
    #[tokio::test]
    async fn a_wheel_moving_at_connect_reports_moving_then_adopts_its_slot() {
        let handle = Arc::new(MockFilterWheelHandle::new("SIM-QHY178M", 7));
        handle.reports_moving.store(true, Ordering::SeqCst);
        handle.defer_move.store(true, Ordering::SeqCst);
        handle.set_position(4).unwrap();
        let device = QhyFilterWheelDevice::new(handle.clone(), None, None);

        device.connect().await.unwrap();
        assert_eq!(device.position().await.unwrap(), None);
        assert_eq!(device.names().await.unwrap().len(), 7);

        handle.complete_move();
        assert_eq!(device.position().await.unwrap(), Some(4));
    }

    // --- FW7: the status in transit, and a connection's first move ----------

    /// The Linux SDK names the slot commanded before a move for as long as the
    /// move travels, and slot 0 before any. Sent straight to slot 0, a
    /// connection's first move would read as arrived at once.
    #[tokio::test]
    async fn a_first_move_to_slot_zero_reads_moving_until_the_wheel_arrives() {
        let handle = Arc::new(MockFilterWheelHandle::new("SIM-QHY178M", 7));
        handle.set_reported_position(3);
        handle.defer_move.store(true, Ordering::SeqCst);
        let device = QhyFilterWheelDevice::new(handle.clone(), None, None);
        device.connect().await.unwrap();

        device.set_position(0).await.unwrap();

        assert_eq!(
            device.position().await.unwrap(),
            None,
            "the wheel is still on its way to slot 0"
        );
        handle.complete_move();
        assert_eq!(device.position().await.unwrap(), Some(0));
    }

    #[tokio::test]
    async fn a_connections_first_move_goes_through_the_slot_the_wheel_stands_on() {
        let handle = Arc::new(MockFilterWheelHandle::new("SIM-QHY178M", 7));
        handle.set_reported_position(3);
        let device = QhyFilterWheelDevice::new(handle.clone(), None, None);
        device.connect().await.unwrap();
        let reads_before = handle.get_position_calls.load(Ordering::SeqCst);

        device.set_position(5).await.unwrap();

        assert_eq!(handle.commands(), vec![3, 5]);
        assert_eq!(
            handle.get_position_calls.load(Ordering::SeqCst),
            reads_before + 1,
            "one read confirmed the slot the wheel stands on"
        );
        let confirmed = *handle.reads_returned().last().unwrap();
        let sent = *handle.moves_sent().last().unwrap();
        assert!(
            sent.duration_since(confirmed) >= REST_AFTER_STATUS_READ,
            "the move went out {:?} after the read that confirmed the prime",
            sent.duration_since(confirmed)
        );
    }

    #[tokio::test]
    async fn a_later_move_goes_straight_to_its_slot() {
        let handle = Arc::new(MockFilterWheelHandle::new("SIM-QHY178M", 7));
        let device = QhyFilterWheelDevice::new(handle.clone(), None, None);
        device.connect().await.unwrap();
        device.set_position(3).await.unwrap();
        assert_eq!(device.position().await.unwrap(), Some(3));

        device.set_position(5).await.unwrap();

        assert_eq!(handle.commands(), vec![0, 3, 5]);
    }

    /// The SDK keeps the slot it was last sent across a close and re-open, but
    /// a connection does not take that on trust.
    #[tokio::test]
    async fn a_reconnect_primes_its_first_move_again() {
        let handle = Arc::new(MockFilterWheelHandle::new("SIM-QHY178M", 7));
        let device = QhyFilterWheelDevice::new(handle.clone(), None, None);
        device.set_connected(true).await.unwrap();
        device.set_position(3).await.unwrap();
        assert_eq!(device.position().await.unwrap(), Some(3));
        device.set_connected(false).await.unwrap();
        device.set_connected(true).await.unwrap();

        device.set_position(5).await.unwrap();

        assert_eq!(handle.commands(), vec![0, 3, 3, 5]);
    }

    /// The Windows SDK passes the CFW's `'N'` through, so a wheel that has
    /// reported itself moving never names a slot in transit, on any connection.
    #[tokio::test]
    async fn a_wheel_that_has_reported_itself_moving_is_not_primed_again() {
        let handle = Arc::new(MockFilterWheelHandle::new("SIM-QHY178M", 7));
        handle.reports_moving.store(true, Ordering::SeqCst);
        handle.defer_move.store(true, Ordering::SeqCst);
        let device = QhyFilterWheelDevice::new(handle.clone(), None, None);
        device.set_connected(true).await.unwrap();
        device.set_position(3).await.unwrap();
        assert_eq!(device.position().await.unwrap(), None);
        handle.complete_move();
        assert_eq!(device.position().await.unwrap(), Some(3));
        device.set_connected(false).await.unwrap();
        device.set_connected(true).await.unwrap();

        device.set_position(5).await.unwrap();

        assert_eq!(handle.commands(), vec![0, 3, 5]);
    }

    /// A wheel whose status does not name the slot it was sent back to is not
    /// where the driver thinks; the move is refused rather than sent blind.
    #[tokio::test]
    async fn a_prime_the_status_does_not_confirm_refuses_the_move() {
        let handle = Arc::new(MockFilterWheelHandle::new("SIM-QHY178M", 7));
        let device = QhyFilterWheelDevice::new(handle.clone(), None, None);
        device.connect().await.unwrap();
        handle.override_status(Some(CfwStatus::Slot(2)));

        let err = device.set_position(3).await.unwrap_err();

        assert_eq!(err.code, ASCOMErrorCode::INVALID_OPERATION);
        assert_eq!(handle.commands(), vec![0], "the move itself was not sent");
        assert_eq!(device.position().await.unwrap(), Some(0));

        // The next move is primed again.
        handle.override_status(None);
        device.set_position(3).await.unwrap();
        assert_eq!(handle.commands(), vec![0, 0, 3]);
    }

    /// With no slot read at connect there is no slot to send the wheel back
    /// to, so the move goes out as it is.
    #[tokio::test]
    async fn a_wheel_whose_slot_the_connect_could_not_read_is_sent_its_move_unprimed() {
        let handle = Arc::new(MockFilterWheelHandle::new("SIM-QHY178M", 7));
        handle.set_reported_position(30);
        let device = QhyFilterWheelDevice::new(handle.clone(), None, None);
        device.connect().await.unwrap();

        device.set_position(3).await.unwrap();

        assert_eq!(handle.commands(), vec![3]);
    }

    // --- FW2, FW8: a write while a move is under way, and a move's deadline --

    /// A CFW drops a move sent while it travels (FW5), so a write while a
    /// move is under way is refused, and the wheel goes on to the slot under
    /// way.
    #[tokio::test]
    async fn a_write_while_a_move_is_under_way_is_refused() {
        let handle = Arc::new(MockFilterWheelHandle::new("SIM-QHY178M", 7));
        handle.defer_move.store(true, Ordering::SeqCst);
        let device = QhyFilterWheelDevice::new(handle.clone(), None, None);
        device.connect().await.unwrap();
        device.set_position(3).await.unwrap();

        let err = device.set_position(5).await.unwrap_err();

        assert_eq!(err.code, ASCOMErrorCode::INVALID_OPERATION);
        assert_eq!(err.message, "the filter wheel is still moving to slot 3");
        assert_eq!(
            handle.commands(),
            vec![0, 3],
            "the refused slot was not sent"
        );
        handle.complete_move();
        assert_eq!(device.position().await.unwrap(), Some(3));
        device.set_position(5).await.unwrap();
        assert_eq!(handle.commands(), vec![0, 3, 5]);
    }

    #[tokio::test]
    async fn a_write_of_the_slot_under_way_is_accepted_and_not_sent_again() {
        let handle = Arc::new(MockFilterWheelHandle::new("SIM-QHY178M", 7));
        handle.defer_move.store(true, Ordering::SeqCst);
        let device = QhyFilterWheelDevice::new(handle.clone(), None, None);
        device.connect().await.unwrap();
        device.set_position(3).await.unwrap();

        device.set_position(3).await.unwrap();

        assert_eq!(handle.commands(), vec![0, 3]);
    }

    /// The range check comes first, so an out-of-range slot is the client's
    /// error whatever the wheel is doing.
    #[tokio::test]
    async fn an_out_of_range_write_while_a_move_is_under_way_is_an_invalid_value() {
        let handle = Arc::new(MockFilterWheelHandle::new("SIM-QHY178M", 7));
        handle.defer_move.store(true, Ordering::SeqCst);
        let device = QhyFilterWheelDevice::new(handle.clone(), None, None);
        device.connect().await.unwrap();
        device.set_position(3).await.unwrap();

        let err = device.set_position(7).await.unwrap_err();

        assert_eq!(err.code, ASCOMErrorCode::INVALID_VALUE);
    }

    /// Long enough for a test's first read to land inside it, short enough
    /// to wait out.
    const TEST_DEADLINE: Duration = Duration::from_millis(100);

    async fn connected_with_test_deadline(
        handle: &Arc<MockFilterWheelHandle>,
    ) -> QhyFilterWheelDevice {
        let device =
            QhyFilterWheelDevice::new(Arc::<MockFilterWheelHandle>::clone(handle), None, None)
                .with_move_deadline(TEST_DEADLINE);
        device.connect().await.unwrap();
        device
    }

    /// A dropped move leaves the wheel at rest where it was. It reads as
    /// moving until its deadline, and then as a failure naming the slot asked
    /// for and the slot the status names.
    #[tokio::test]
    async fn a_move_that_has_not_arrived_by_its_deadline_fails() {
        let handle = Arc::new(MockFilterWheelHandle::new("SIM-QHY178M", 7));
        handle.drop_next_move.store(true, Ordering::SeqCst);
        let device = connected_with_test_deadline(&handle).await;
        device.set_position(3).await.unwrap();
        assert_eq!(device.position().await.unwrap(), None);

        tokio::time::sleep(TEST_DEADLINE).await;
        let err = device.position().await.unwrap_err();

        assert_eq!(err.code, UNSPECIFIED_ERROR);
        assert_eq!(
            err.message,
            "the filter wheel did not reach slot 3 within 100ms; its status names slot 0"
        );
    }

    #[tokio::test]
    async fn a_failed_move_is_reported_until_the_next_write() {
        let handle = Arc::new(MockFilterWheelHandle::new("SIM-QHY178M", 7));
        handle.drop_next_move.store(true, Ordering::SeqCst);
        let device = connected_with_test_deadline(&handle).await;
        device.set_position(3).await.unwrap();
        tokio::time::sleep(TEST_DEADLINE).await;
        device.position().await.unwrap_err();
        let reads = handle.get_position_calls.load(Ordering::SeqCst);

        assert_eq!(device.position().await.unwrap_err().code, UNSPECIFIED_ERROR);
        assert_eq!(
            handle.get_position_calls.load(Ordering::SeqCst),
            reads,
            "a failed move is reported without reading the wheel"
        );
        device.set_position(5).await.unwrap();
        assert_eq!(device.position().await.unwrap(), Some(5));
    }

    /// In transit the Linux SDK names the slot it was last sent, which after a
    /// failure is the failed move's own: sent straight, it would read as
    /// arrived at once. So it goes through the slot the wheel stands on (FW7).
    #[tokio::test]
    async fn the_failed_slot_is_sent_again_through_the_slot_the_wheel_stands_on() {
        let handle = Arc::new(MockFilterWheelHandle::new("SIM-QHY178M", 7));
        handle.drop_next_move.store(true, Ordering::SeqCst);
        let device = connected_with_test_deadline(&handle).await;
        device.set_position(3).await.unwrap();
        tokio::time::sleep(TEST_DEADLINE).await;
        device.position().await.unwrap_err();
        handle.defer_move.store(true, Ordering::SeqCst);

        device.set_position(3).await.unwrap();

        assert_eq!(handle.commands(), vec![0, 3, 0, 3]);
        assert_eq!(
            device.position().await.unwrap(),
            None,
            "the wheel is on its way to slot 3"
        );
        handle.complete_move();
        assert_eq!(device.position().await.unwrap(), Some(3));
    }

    /// A wheel that reports itself moving names no slot in transit, so the
    /// failed slot goes out again as it is (FW7).
    #[tokio::test]
    async fn on_a_wheel_that_reports_itself_moving_the_failed_slot_is_sent_again_as_it_is() {
        let handle = Arc::new(MockFilterWheelHandle::new("SIM-QHY178M", 7));
        handle.reports_moving.store(true, Ordering::SeqCst);
        handle.defer_move.store(true, Ordering::SeqCst);
        let device = connected_with_test_deadline(&handle).await;
        device.set_position(2).await.unwrap();
        assert_eq!(device.position().await.unwrap(), None);
        handle.complete_move();
        assert_eq!(device.position().await.unwrap(), Some(2));
        handle.drop_next_move.store(true, Ordering::SeqCst);
        device.set_position(3).await.unwrap();
        tokio::time::sleep(TEST_DEADLINE).await;
        assert_eq!(
            device.position().await.unwrap_err().message,
            "the filter wheel did not reach slot 3 within 100ms; its status names slot 2"
        );

        device.set_position(3).await.unwrap();

        assert_eq!(handle.commands(), vec![0, 2, 3, 3]);
    }

    /// Where a wheel stands when a failed move's slot is sent again need not
    /// be where the driver last knew it: a CFW whose power comes back homes to
    /// slot 0, and under Linux the status of a wheel that has not stopped
    /// names the slot sent before the move. Sent back to a slot it is not on,
    /// the prime would move it, so the status is read afresh.
    #[tokio::test]
    async fn the_failed_slot_goes_through_the_slot_the_wheel_stands_on_when_it_is_sent_again() {
        let handle = Arc::new(MockFilterWheelHandle::new("SIM-QHY178M", 7));
        let device = connected_with_test_deadline(&handle).await;
        device.set_position(2).await.unwrap();
        assert_eq!(device.position().await.unwrap(), Some(2));
        handle.drop_next_move.store(true, Ordering::SeqCst);
        device.set_position(3).await.unwrap();
        tokio::time::sleep(TEST_DEADLINE).await;
        device.position().await.unwrap_err();
        handle.set_reported_position(0);

        device.set_position(3).await.unwrap();

        assert_eq!(handle.commands(), vec![0, 2, 3, 0, 3]);
    }

    /// The failure is reported until the client acts on it, not until the
    /// client's next move succeeds: a write that fails leaves `Position`
    /// reading the wheel.
    #[tokio::test]
    async fn a_write_that_fails_after_a_failed_move_leaves_position_reading_the_wheel() {
        let handle = Arc::new(MockFilterWheelHandle::new("SIM-QHY178M", 7));
        handle.drop_next_move.store(true, Ordering::SeqCst);
        let device = connected_with_test_deadline(&handle).await;
        device.set_position(3).await.unwrap();
        tokio::time::sleep(TEST_DEADLINE).await;
        device.position().await.unwrap_err();
        handle.fail_next_move.store(true, Ordering::SeqCst);

        let err = device.set_position(5).await.unwrap_err();

        assert_eq!(err.code, ASCOMErrorCode::INVALID_OPERATION);
        assert_eq!(device.position().await.unwrap(), Some(0));
    }

    /// A write that comes after the deadline, before any read saw it pass, is
    /// not refused on behalf of a move that has already failed.
    #[tokio::test]
    async fn a_write_after_the_deadline_goes_out_though_no_read_saw_it_pass() {
        let handle = Arc::new(MockFilterWheelHandle::new("SIM-QHY178M", 7));
        handle.drop_next_move.store(true, Ordering::SeqCst);
        let device = connected_with_test_deadline(&handle).await;
        device.set_position(3).await.unwrap();
        tokio::time::sleep(TEST_DEADLINE).await;

        device.set_position(5).await.unwrap();

        assert_eq!(handle.commands(), vec![0, 3, 5]);
        assert_eq!(device.position().await.unwrap(), Some(5));
    }

    /// A move whose status names its slot at the first read past the deadline
    /// has arrived; the deadline is for a wheel that has not.
    #[tokio::test]
    async fn a_move_named_arrived_by_the_first_read_past_its_deadline_has_not_failed() {
        let handle = Arc::new(MockFilterWheelHandle::new("SIM-QHY178M", 7));
        handle.defer_move.store(true, Ordering::SeqCst);
        let device = connected_with_test_deadline(&handle).await;
        device.set_position(3).await.unwrap();
        tokio::time::sleep(TEST_DEADLINE).await;
        handle.complete_move();

        assert_eq!(device.position().await.unwrap(), Some(3));
    }

    /// Two polls can race past the deadline: the one whose reading lands
    /// after the other recorded the failure reports it too, rather than
    /// adopting the slot it read as where the wheel was asked to go.
    #[tokio::test]
    async fn a_reading_taken_after_a_move_failed_reports_the_failure() {
        let handle = Arc::new(MockFilterWheelHandle::new("SIM-QHY178M", 7));
        handle.drop_next_move.store(true, Ordering::SeqCst);
        let device = connected_with_test_deadline(&handle).await;
        device.set_position(3).await.unwrap();
        tokio::time::sleep(TEST_DEADLINE).await;
        device.position().await.unwrap_err();

        let reading = device
            .state
            .take_reading(CfwStatus::Slot(0), 7, TEST_DEADLINE);

        assert!(reading.unwrap_err().contains("did not reach slot 3"));
        assert_eq!(*device.state.target_position.lock(), None);
    }

    /// The move under way is the connection's: after a reconnect, which reads
    /// the wheel afresh, a write is not refused on its behalf.
    #[tokio::test]
    async fn a_reconnect_forgets_the_move_under_way() {
        let handle = Arc::new(MockFilterWheelHandle::new("SIM-QHY178M", 7));
        handle.defer_move.store(true, Ordering::SeqCst);
        let device =
            QhyFilterWheelDevice::new(Arc::<MockFilterWheelHandle>::clone(&handle), None, None);
        device.set_connected(true).await.unwrap();
        device.set_position(3).await.unwrap();
        handle.complete_move();
        device.set_connected(false).await.unwrap();
        device.set_connected(true).await.unwrap();

        device.set_position(5).await.unwrap();

        assert_eq!(handle.commands(), vec![0, 3, 3, 5]);
    }

    /// A failed move is the connection's: a reconnect reads the wheel afresh.
    #[tokio::test]
    async fn a_reconnect_clears_a_failed_move() {
        let handle = Arc::new(MockFilterWheelHandle::new("SIM-QHY178M", 7));
        handle.drop_next_move.store(true, Ordering::SeqCst);
        let device =
            QhyFilterWheelDevice::new(Arc::<MockFilterWheelHandle>::clone(&handle), None, None)
                .with_move_deadline(TEST_DEADLINE);
        device.set_connected(true).await.unwrap();
        device.set_position(3).await.unwrap();
        tokio::time::sleep(TEST_DEADLINE).await;
        device.position().await.unwrap_err();

        device.set_connected(false).await.unwrap();
        device.set_connected(true).await.unwrap();

        assert_eq!(device.position().await.unwrap(), Some(0));
    }

    #[tokio::test]
    async fn a_slot_past_the_sdk_word_is_rejected_not_wrapped() {
        // The slot arrives from the client as a `usize`. Narrowing it to the
        // SDK's `u32` before the range check turned every value past 2^32 into
        // one the wheel would happily move to: 4_294_967_299 is 2^32 + 3, which
        // used to truncate to slot 3 and pass a `0..7` range check.
        let device = connected(None).await;

        let err = device.set_position(4_294_967_299).await.unwrap_err();

        assert_eq!(err.code, ASCOMErrorCode::INVALID_VALUE);
        assert_eq!(
            device.position().await.unwrap(),
            Some(0),
            "a rejected slot must leave the wheel where it was"
        );
    }

    #[tokio::test]
    async fn focus_offsets_are_zero_per_filter() {
        let device = connected(None).await;
        assert_eq!(device.focus_offsets().await.unwrap(), vec![0; 7]);
    }

    #[tokio::test]
    async fn unique_id_is_prefixed() {
        let device = connected(None).await;
        assert_eq!(device.unique_id(), "CFW-SIM-QHY178M");
    }

    // --- FW4: a wheel whose camera has left the bus ---------------------------

    /// The wheel's own failed call asks C9's question too, so a wheel whose
    /// camera has gone reads disconnected rather than answering slot errors.
    #[tokio::test]
    async fn a_wheel_whose_camera_left_the_bus_reads_disconnected_after_its_next_failure() {
        let handle = Arc::new(MockFilterWheelHandle::new("SIM-QHY178M", 7));
        let device =
            QhyFilterWheelDevice::new(Arc::<MockFilterWheelHandle>::clone(&handle), None, None);
        device.set_connected(true).await.unwrap();
        handle.leave_bus();

        let err = device.set_position(3).await.unwrap_err();

        assert_eq!(err.code, ASCOMErrorCode::NOT_CONNECTED);
        assert!(!device.connected().await.unwrap());
        assert_eq!(
            device.names().await.unwrap_err().code,
            ASCOMErrorCode::NOT_CONNECTED
        );
    }

    /// A failed wheel request answers from the verdict its own question
    /// returned (C9, FW4), not from a read of the connection a reconnect may
    /// already have replaced.
    #[tokio::test]
    async fn a_wheel_failure_answers_from_its_own_verdict_even_if_a_reconnect_lands_after_it() {
        let handle = Arc::new(MockFilterWheelHandle::new("SIM-QHY178M", 7));
        let device =
            QhyFilterWheelDevice::new(Arc::<MockFilterWheelHandle>::clone(&handle), None, None);
        device.set_connected(true).await.unwrap();
        handle.leave_bus();
        handle
            .reconnect_lands_after_verdict
            .store(true, Ordering::SeqCst);

        let err = device.set_position(3).await.unwrap_err();

        assert_eq!(err.code, ASCOMErrorCode::NOT_CONNECTED);
        assert!(
            device.connected().await.unwrap(),
            "the connection reads healthy again, as after a reconnect"
        );
    }

    /// A wheel request whose handle a reconnect replaced between its failed
    /// call and its question answers `NOT_CONNECTED` (C9, FW4).
    #[tokio::test]
    async fn a_wheel_failure_on_a_handle_replaced_before_its_question_answers_not_connected() {
        let handle = Arc::new(MockFilterWheelHandle::new("SIM-QHY178M", 7));
        let device =
            QhyFilterWheelDevice::new(Arc::<MockFilterWheelHandle>::clone(&handle), None, None);
        device.set_connected(true).await.unwrap();
        handle.leave_bus();
        handle
            .reconnect_lands_before_verdict
            .store(true, Ordering::SeqCst);

        let err = device.set_position(3).await.unwrap_err();

        assert_eq!(err.code, ASCOMErrorCode::NOT_CONNECTED);
    }

    /// `Connected = false` releases a wheel whose camera has gone (FW4).
    #[tokio::test]
    async fn disconnecting_a_wheel_whose_camera_left_releases_it() {
        let handle = Arc::new(MockFilterWheelHandle::new("SIM-QHY178M", 7));
        let device =
            QhyFilterWheelDevice::new(Arc::<MockFilterWheelHandle>::clone(&handle), None, None);
        device.set_connected(true).await.unwrap();
        handle.leave_bus();
        device.set_position(3).await.unwrap_err();

        device.set_connected(false).await.unwrap();

        assert!(!handle.is_open().unwrap());
        assert!(!device.connected().await.unwrap());
    }
}
