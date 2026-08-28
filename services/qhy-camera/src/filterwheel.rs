//! `QhyFilterWheelDevice` — the ASCOM `Device` + `FilterWheel` implementation
//! over the [`FilterWheelHandle`](crate::backend::FilterWheelHandle) seam.
//!
//! Registered automatically, one per discovered CFW (detection is the source of
//! truth — there is no opt-in toggle). `Names`
//! are the configured `filter_names` or generated `Filter0..N`; `Position`
//! returns `None` while the commanded target differs from the actual slot (ASCOM
//! "moving" sentinel); `FocusOffsets` is zero per filter in v0.

use std::sync::Arc;

use ascom_alpaca::api::{Device, FilterWheel};
use ascom_alpaca::{ASCOMError, ASCOMResult};
use parking_lot::Mutex;
use tracing::debug;

use crate::backend::FilterWheelHandle;

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
            }),
        }
    }

    fn ensure_connected(&self) -> ASCOMResult<()> {
        match self.handle.is_open() {
            Ok(true) => Ok(()),
            _ => Err(ASCOMError::NOT_CONNECTED),
        }
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
    async fn on_handle<T, F>(&self, f: F) -> ASCOMResult<T>
    where
        F: FnOnce(&dyn FilterWheelHandle) -> ASCOMResult<T> + Send + 'static,
        T: Send + 'static,
    {
        let handle = Arc::clone(&self.handle);
        let outcome = tokio::task::spawn_blocking(move || f(handle.as_ref()))
            .await
            .map_err(|e| ASCOMError::invalid_operation(format!("SDK task failed: {e}")))?;
        match outcome {
            Err(e) if self.ensure_connected().is_err() => {
                debug!(error = %e, "SDK call failed on a handle that is no longer open");
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
        let position = self
            .handle
            .get_position()
            .map_err(|_| ASCOMError::NOT_CONNECTED)?;
        // The slot count sizes `Names` and `FocusOffsets`, so a wheel reporting
        // one this target cannot address has not handshaken.
        let Ok(count) = usize::try_from(count) else {
            return Err(ASCOMError::NOT_CONNECTED);
        };
        // The slot is different: `cfw_ascii_to_slot` degrades a nonstandard CFW
        // status byte into `byte - 0x30` rather than failing, so a value outside
        // the wheel's own count is what a wheel that is not reporting a slot —
        // one still moving, most likely — looks like from here. ASCOM's answer
        // for that is the moving sentinel (`Position` = -1), not a refused
        // connect, so cache no slot and let `position` adopt one as soon as the
        // wheel reports a real one.
        let settled = usize::try_from(position).ok().filter(|slot| *slot < count);
        if settled.is_none() {
            debug!(
                filter_wheel = %self.unique_id,
                slots = count,
                reported = position,
                "CFW reported no readable slot at connect; Position stays the moving sentinel until it does"
            );
        }
        *self.state.number_of_filters.lock() = Some(count);
        *self.state.target_position.lock() = settled;
        *self.state.settled_position.lock() = settled;
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
        // open/close.
        Ok(self.handle.is_open().unwrap_or_else(|e| {
            debug!(filter_wheel = %self.unique_id, error = %e, "is_open() failed; reporting disconnected");
            false
        }))
    }

    async fn set_connected(&self, connected: bool) -> ASCOMResult<()> {
        let current = self
            .handle
            .is_open()
            .map_err(|_| ASCOMError::NOT_CONNECTED)?;
        if current == connected {
            return Ok(());
        }
        if connected {
            self.connect().await
        } else {
            self.disconnect().await
        }
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

        let actual = self
            .on_handle(|h| h.get_position().map_err(|_| ASCOMError::INVALID_OPERATION))
            .await?;
        // The SDK answers in `u32` and decodes any nonstandard status byte to
        // `byte - 0x30`, so a slot outside the wheel's own count is a status
        // that does not name a slot rather than a slot to report.
        let actual = usize::try_from(actual).ok().filter(|slot| *slot < count);

        // `None` is the ASCOM "moving" sentinel (`Position` = -1).
        match (actual, target) {
            // Reached the commanded slot.
            (Some(actual), Some(target)) if actual == target => {
                *self.state.settled_position.lock() = Some(actual);
                Ok(Some(actual))
            }
            // Connect could not read a slot, so there is nothing commanded to
            // reach — adopt the first real one the wheel reports.
            (Some(actual), None) => {
                *self.state.target_position.lock() = Some(actual);
                *self.state.settled_position.lock() = Some(actual);
                Ok(Some(actual))
            }
            // Still travelling, or still not naming a slot.
            _ => Ok(None),
        }
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
        if *self.state.target_position.lock() == Some(position) {
            return Ok(());
        }
        // `position < count`, and the count itself came from an SDK `u32`.
        let target = u32::try_from(position).map_err(|_| ASCOMError::INVALID_OPERATION)?;
        self.on_handle(move |h| {
            h.set_position(target)
                .map_err(|_| ASCOMError::INVALID_OPERATION)
        })
        .await?;
        *self.state.target_position.lock() = Some(position);
        Ok(())
    }
}

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
mod tests {
    use super::*;
    use crate::backend::mock::MockFilterWheelHandle;
    use ascom_alpaca::ASCOMErrorCode;
    use std::sync::atomic::Ordering;

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
        // `cfw_ascii_to_slot` decodes any nonstandard CFW status byte as
        // `byte - 0x30`, which for anything past 'F' lands outside the wheel's
        // slot count — 'N' (0x4E) decodes to 30 on a 7-slot wheel. That is a
        // status which does not name a slot, most likely a wheel still moving.
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
}
