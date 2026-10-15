//! rp's train lease (rp.md § Train Lease).
//!
//! A lease is what a training run on an optical train holds for its
//! whole length, so neither rp's own work on the train's light path,
//! nor mount motion, nor PHD2 guiding can land between two of its
//! samples.
//!
//! [`TrainLeases`] is the book: the ids rp has seen, the reservation
//! table that leases and rp's own `auto_focus` sweeps share, and every
//! live [`Lease`]. The acquire's five steps run in the tool body
//! (`mcp::built_in::train_lease`), which pushes each slot and the gate
//! permit into the lease as it takes them. Each lease has a keeper task,
//! spawned at its reservation, that renews the facade lease, ends the
//! lease idle or at its `max_duration`, and gives everything back once
//! the lease has ended and nothing runs under it.

pub mod facade;
#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
pub(crate) mod test_support;

use std::collections::{BTreeSet, HashMap};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::Duration;

use ascom_alpaca::api::Camera;
use tokio::sync::{Notify, OwnedRwLockReadGuard};
use tokio::time::Instant;
use tokio_util::sync::CancellationToken;
use tracing::{debug, warn};

use crate::config::optical_train::TrainPurpose;
use crate::equipment::trains::{TrainDeviceKind, TrainModel};
use crate::equipment::{DeviceSlot, SlotGuard};

/// How long rp remembers an id after its lease ends, or after a release
/// recorded it: longer than the longest wait before a grant plus
/// `max_duration`.
pub const SEEN_ID_MEMORY: Duration = Duration::from_hours(1);
/// The most ids rp remembers at once, live leases included.
pub const SEEN_ID_CAP: usize = 4096;
/// How long a granted lease lives with no call under it in flight.
pub const IDLE_TIMEOUT: Duration = Duration::from_mins(1);
/// A lease's life when the acquire names none.
pub const DEFAULT_MAX_DURATION: Duration = Duration::from_mins(5);
/// The longest `max_duration` an acquire may ask for.
pub const MAX_DURATION_CEILING: Duration = Duration::from_mins(10);
/// The longest lease id, in characters.
const LEASE_ID_MAX_LEN: usize = 128;

/// Check a caller's lease id: 1 to 128 printable ASCII characters, no
/// spaces.
///
/// # Errors
///
/// What is wrong with the id.
pub fn check_lease_id(lease_id: &str) -> Result<(), String> {
    if (1..=LEASE_ID_MAX_LEN).contains(&lease_id.len())
        && lease_id.bytes().all(|b| b.is_ascii_graphic())
    {
        Ok(())
    } else {
        Err(format!(
            "lease_id must be 1 to {LEASE_ID_MAX_LEN} printable ASCII characters without spaces, got {lease_id:?}"
        ))
    }
}

/// Why a lease ended.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum LeaseEnd {
    Released,
    Idle,
    MaxDuration,
    /// The facade refused a renewal: its lease is no longer held.
    FacadeLost(String),
    Safety,
    Shutdown,
    /// Its acquire failed or was cancelled before the grant.
    AcquireFailed,
}

impl LeaseEnd {
    /// The reason a call under the ended lease is told.
    #[must_use]
    pub fn why(&self) -> String {
        match self {
            Self::Released => "it was released".to_string(),
            Self::Idle => format!(
                "it ended after {} s with no call under it",
                IDLE_TIMEOUT.as_secs()
            ),
            Self::MaxDuration => "it reached its max_duration".to_string(),
            Self::FacadeLost(reason) => {
                format!("it lost the camera facade's sweep lease ({reason})")
            }
            Self::Safety => "the safety transition ended it".to_string(),
            Self::Shutdown => "rp is shutting down".to_string(),
            Self::AcquireFailed => "its acquire did not complete".to_string(),
        }
    }
}

fn not_held(lease_id: &str, why: &str) -> String {
    format!("lease '{lease_id}' is not held: {why}")
}

/// What a lease on one train covers (rp.md § Train Lease, "What the
/// lease covers"), derived from the train model.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LeasePlan {
    pub train_id: String,
    /// A guiding train's lease takes the camera facade's sweep lease.
    pub guiding: bool,
    pub camera_id: String,
    /// The terminal focuser: the one a training run moves.
    pub focuser_id: String,
    /// Every filter wheel in the train, in optical order; moved too.
    pub filter_wheel_ids: Vec<String>,
    /// Every other focuser in the train, in optical order; held still.
    pub held_focuser_ids: Vec<String>,
    /// Every rotator in the train, in optical order; held still.
    pub held_rotator_ids: Vec<String>,
    /// Every other camera in a train containing a device the lease
    /// moves, in camera-id order; drained and held.
    pub drained_camera_ids: Vec<String>,
}

impl LeasePlan {
    /// The plan for a lease on `train_id`.
    ///
    /// # Errors
    ///
    /// An unknown train, or one without a focuser.
    pub fn for_train(trains: &TrainModel, train_id: &str) -> Result<Self, String> {
        let train = trains
            .train(train_id)
            .ok_or_else(|| format!("train not found: {train_id}"))?;
        let camera_id = train
            .camera_id()
            .ok_or_else(|| format!("train '{train_id}' has no camera"))?;
        let focuser_id = train
            .terminal_focuser()
            .ok_or_else(|| format!("train '{train_id}' has no focuser"))?;
        let owned = |ids: Vec<&str>| ids.into_iter().map(str::to_string).collect::<Vec<_>>();
        let filter_wheel_ids = owned(train.ids_of_kind(TrainDeviceKind::FilterWheel));
        let held_focuser_ids = owned(
            train
                .ids_of_kind(TrainDeviceKind::Focuser)
                .into_iter()
                .filter(|id| *id != focuser_id)
                .collect(),
        );
        let held_rotator_ids = owned(train.ids_of_kind(TrainDeviceKind::Rotator));
        let moved: Vec<&str> = std::iter::once(focuser_id)
            .chain(filter_wheel_ids.iter().map(String::as_str))
            .collect();
        let drained_camera_ids: BTreeSet<String> = trains
            .trains()
            .iter()
            .filter(|t| t.devices.iter().any(|d| moved.contains(&d.id.as_str())))
            .filter_map(|t| t.camera_id())
            .filter(|id| *id != camera_id)
            .map(str::to_string)
            .collect();
        Ok(Self {
            train_id: train_id.to_string(),
            guiding: train.purpose == TrainPurpose::Guiding,
            camera_id: camera_id.to_string(),
            focuser_id: focuser_id.to_string(),
            filter_wheel_ids,
            held_focuser_ids,
            held_rotator_ids,
            drained_camera_ids: drained_camera_ids.into_iter().collect(),
        })
    }

    /// The cameras the lease holds: its own, then the drained ones.
    pub fn cameras(&self) -> impl Iterator<Item = &str> {
        std::iter::once(self.camera_id.as_str())
            .chain(self.drained_camera_ids.iter().map(String::as_str))
    }

    /// Whether a call under the lease may move `device_id`.
    #[must_use]
    pub fn moves(&self, device_id: &str) -> bool {
        self.focuser_id == device_id || self.filter_wheel_ids.iter().any(|id| id == device_id)
    }

    /// Whether the lease holds `device_id` still.
    #[must_use]
    pub fn holds_still(&self, device_id: &str) -> bool {
        self.held_focuser_ids
            .iter()
            .chain(&self.held_rotator_ids)
            .any(|id| id == device_id)
    }

    /// Every device the lease moves or holds still, with its kind.
    pub fn devices(&self) -> impl Iterator<Item = (&'static str, &str)> {
        std::iter::once(("focuser", self.focuser_id.as_str()))
            .chain(
                self.filter_wheel_ids
                    .iter()
                    .map(|id| ("filter wheel", id.as_str())),
            )
            .chain(
                self.held_focuser_ids
                    .iter()
                    .map(|id| ("focuser", id.as_str())),
            )
            .chain(
                self.held_rotator_ids
                    .iter()
                    .map(|id| ("rotator", id.as_str())),
            )
    }

    /// Every camera and device in the reservation, with its kind.
    fn reservation(&self) -> impl Iterator<Item = (&'static str, &str)> {
        self.cameras()
            .map(|id| ("camera", id))
            .chain(self.devices())
    }

    /// Whether the reservation includes `id`, a camera or a device.
    #[must_use]
    pub fn reserves(&self, id: &str) -> bool {
        self.reservation().any(|(_, reserved)| reserved == id)
    }

    /// A camera or device both reservations include, with its kind.
    fn shared_with(&self, other: &Self) -> Option<(&'static str, &str)> {
        self.reservation().find(|(_, id)| other.reserves(id))
    }
}

/// A call under a lease: a capture through a camera it holds, or a move
/// or filter change of a device it moves (rp.md § Train Lease, "Calls
/// under the lease").
#[derive(Clone, Copy, Debug)]
pub enum CallKind<'a> {
    Capture {
        camera_id: &'a str,
    },
    /// `kind` names the device in a refusal ("focuser", "filter wheel").
    Move {
        kind: &'static str,
        device_id: &'a str,
    },
}

#[derive(Debug)]
enum Phase {
    Acquiring,
    Granted { ends_by: Instant },
    Ending(LeaseEnd),
}

/// The facade lease a guiding train's lease took.
struct FacadeHold {
    camera: Arc<dyn Camera>,
    token: String,
    /// When the next renewal is due; `None` until the facade granted it.
    next_renewal: Option<Instant>,
}

#[derive(Default)]
struct Holdings {
    slots: Vec<SlotGuard>,
    permit: Option<OwnedRwLockReadGuard<()>>,
}

struct LeaseState {
    phase: Phase,
    /// Whether the acquire's body is still running; the keeper gives
    /// nothing back before it has returned, so no slot it takes late is
    /// left behind.
    acquiring: bool,
    calls: u32,
    /// Captures under the lease in flight that were not cancelled: the
    /// gate permit stays while one is.
    uncancelled_captures: u32,
    /// The grant, or the end of the last call under the lease: what the
    /// idle timeout counts from.
    last_activity: Instant,
    holdings: Holdings,
    facade: Option<FacadeHold>,
}

/// One lease, from its reservation until it has given everything back.
pub struct Lease {
    pub id: String,
    pub plan: LeasePlan,
    state: Mutex<LeaseState>,
    /// Fired when the lease stops admitting calls.
    ending: CancellationToken,
    /// Fired once it has given everything back and left the table.
    released: CancellationToken,
    /// Wakes the keeper when a call starts or ends, the facade lease is
    /// granted, or the lease ends. Only the keeper waits on it, so
    /// `notify_one`'s stored permit loses no wake-up.
    changed: Notify,
    /// The slots calls under the lease take, one per camera and device
    /// it holds, so calls through one of them still run one at a time.
    inner_slots: HashMap<String, DeviceSlot>,
}

impl Lease {
    fn new(id: &str, plan: LeasePlan) -> Self {
        let inner_slots = plan
            .cameras()
            .map(str::to_string)
            .chain(plan.devices().map(|(_, id)| id.to_string()))
            .map(|id| (id, DeviceSlot::default()))
            .collect();
        Self {
            id: id.to_string(),
            plan,
            state: Mutex::new(LeaseState {
                phase: Phase::Acquiring,
                acquiring: true,
                calls: 0,
                uncancelled_captures: 0,
                last_activity: Instant::now(),
                holdings: Holdings::default(),
                facade: None,
            }),
            ending: CancellationToken::new(),
            released: CancellationToken::new(),
            changed: Notify::new(),
            inner_slots,
        }
    }

    fn state(&self) -> MutexGuard<'_, LeaseState> {
        // A poisoned lock only means a holder panicked; the state is
        // still consistent.
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Fired when the lease stops admitting calls.
    #[must_use]
    pub const fn ending(&self) -> &CancellationToken {
        &self.ending
    }

    /// Why the lease ended, or `None` while it lives.
    #[must_use]
    pub fn end_reason(&self) -> Option<LeaseEnd> {
        match &self.state().phase {
            Phase::Ending(end) => Some(end.clone()),
            Phase::Acquiring | Phase::Granted { .. } => None,
        }
    }

    /// Keep a slot until the lease gives everything back.
    pub fn hold(&self, slot: SlotGuard) {
        self.state().holdings.slots.push(slot);
    }

    /// Keep the gate permit while the lease lives and a capture under it
    /// that was not cancelled is in flight.
    pub fn hold_permit(&self, permit: OwnedRwLockReadGuard<()>) {
        let mut state = self.state();
        let refused = if matches!(state.phase, Phase::Ending(_)) && state.uncancelled_captures == 0
        {
            Some(permit)
        } else {
            state.holdings.permit = Some(permit);
            None
        };
        drop(state);
        drop(refused);
    }

    /// Record the facade lease under `token` before asking for it, so
    /// the lease releases it whatever the acquire comes to, even when
    /// the answer never arrives.
    pub fn record_facade(&self, camera: Arc<dyn Camera>, token: String) {
        self.state().facade = Some(FacadeHold {
            camera,
            token,
            next_renewal: None,
        });
    }

    /// The facade granted its lease: renew it from now on.
    pub fn facade_granted(&self) {
        if let Some(facade) = self.state().facade.as_mut() {
            facade.next_renewal = Some(later(Instant::now(), facade::RENEW_INTERVAL));
        }
        self.changed.notify_one();
    }

    /// Grant the lease for `max_duration` from now.
    ///
    /// # Errors
    ///
    /// The end of a lease that ended while its acquire waited.
    pub fn grant(&self, max_duration: Duration) -> Result<(), LeaseEnd> {
        let now = Instant::now();
        let mut state = self.state();
        if let Phase::Ending(end) = &state.phase {
            return Err(end.clone());
        }
        state.phase = Phase::Granted {
            ends_by: later(now, max_duration),
        };
        state.last_activity = now;
        drop(state);
        self.changed.notify_one();
        debug!(lease_id = %self.id, train_id = %self.plan.train_id, ?max_duration, "train lease granted");
        Ok(())
    }

    /// End the lease: it admits no further call. `true` when this call
    /// ended it, `false` when it had already ended.
    pub fn end(&self, end: LeaseEnd) -> bool {
        let mut state = self.state();
        if matches!(state.phase, Phase::Ending(_)) {
            return false;
        }
        let permit = if state.uncancelled_captures == 0 {
            state.holdings.permit.take()
        } else {
            None
        };
        self.log_end(&end);
        state.phase = Phase::Ending(end);
        drop(state);
        drop(permit);
        self.ending.cancel();
        self.changed.notify_one();
        true
    }

    fn log_end(&self, end: &LeaseEnd) {
        if let LeaseEnd::FacadeLost(reason) = end {
            warn!(lease_id = %self.id, train_id = %self.plan.train_id, %reason, "train lease ended: the camera facade no longer holds its sweep lease");
        } else {
            debug!(lease_id = %self.id, train_id = %self.plan.train_id, ?end, "train lease ended");
        }
    }

    /// End the lease when it has reached `ends_by`, or has sat idle for
    /// [`IDLE_TIMEOUT`]. Under the lock, so a call admitted at the same
    /// moment either counts or is refused.
    fn end_if_due(&self, now: Instant) {
        let due = {
            let state = self.state();
            match state.phase {
                Phase::Granted { ends_by } if now >= ends_by => Some(LeaseEnd::MaxDuration),
                Phase::Granted { .. }
                    if state.calls == 0 && now >= later(state.last_activity, IDLE_TIMEOUT) =>
                {
                    Some(LeaseEnd::Idle)
                }
                _ => None,
            }
        };
        if let Some(end) = due {
            self.end(end);
        }
    }

    /// When the keeper must next look at the lease: its end, its idle
    /// timeout, and its facade renewal.
    fn next_wake(&self) -> (Option<Instant>, Option<Instant>) {
        let state = self.state();
        let end = match state.phase {
            Phase::Granted { ends_by } if state.calls == 0 => {
                Some(ends_by.min(later(state.last_activity, IDLE_TIMEOUT)))
            }
            Phase::Granted { ends_by } => Some(ends_by),
            Phase::Acquiring | Phase::Ending(_) => None,
        };
        let renewal = state.facade.as_ref().and_then(|f| f.next_renewal);
        drop(state);
        (end, renewal)
    }

    /// The facade lease to renew now, if one is due, with its next
    /// renewal scheduled.
    fn due_renewal(&self, now: Instant) -> Option<(Arc<dyn Camera>, String)> {
        let mut state = self.state();
        let facade = state.facade.as_mut()?;
        let due = facade.next_renewal.filter(|at| *at <= now)?;
        facade.next_renewal = Some(later(due, facade::RENEW_INTERVAL).max(now));
        let renewal = (facade.camera.clone(), facade.token.clone());
        drop(state);
        Some(renewal)
    }

    fn admit(self: &Arc<Self>, call: CallKind<'_>) -> Result<LeaseCall, String> {
        let now = Instant::now();
        let mut state = self.state();
        match &state.phase {
            Phase::Acquiring => return Err(not_held(&self.id, "its acquire is still waiting")),
            Phase::Ending(end) => return Err(not_held(&self.id, &end.why())),
            Phase::Granted { ends_by } if now >= *ends_by => {
                drop(state);
                self.end(LeaseEnd::MaxDuration);
                return Err(not_held(&self.id, &LeaseEnd::MaxDuration.why()));
            }
            Phase::Granted { .. } => {}
        }
        let (device, capture) = match call {
            CallKind::Capture { camera_id } => {
                if !self.plan.cameras().any(|id| id == camera_id) {
                    return Err(format!(
                        "camera '{camera_id}' is not held by lease '{}'",
                        self.id
                    ));
                }
                (camera_id, true)
            }
            CallKind::Move { kind, device_id } => {
                if self.plan.holds_still(device_id) {
                    return Err(format!(
                        "lease '{}' holds {kind} '{device_id}' still; a call under it cannot move it",
                        self.id
                    ));
                }
                if !self.plan.moves(device_id) {
                    return Err(format!(
                        "{kind} '{device_id}' is not in lease '{}'",
                        self.id
                    ));
                }
                (device_id, false)
            }
        };
        state.calls = state.calls.saturating_add(1);
        if capture {
            state.uncancelled_captures = state.uncancelled_captures.saturating_add(1);
        }
        drop(state);
        self.changed.notify_one();
        Ok(LeaseCall {
            lease: self.clone(),
            device: device.to_string(),
            capture,
            cancelled: false,
        })
    }

    /// A capture under the lease was cancelled mid-exposure: an ending
    /// lease gives the gate back once no uncancelled capture is left.
    fn capture_cancelled(&self) {
        let mut state = self.state();
        state.uncancelled_captures = state.uncancelled_captures.saturating_sub(1);
        let permit = (matches!(state.phase, Phase::Ending(_)) && state.uncancelled_captures == 0)
            .then(|| state.holdings.permit.take())
            .flatten();
        drop(state);
        drop(permit);
    }

    fn call_ended(&self, uncancelled_capture: bool) {
        let mut state = self.state();
        state.calls = state.calls.saturating_sub(1);
        if uncancelled_capture {
            state.uncancelled_captures = state.uncancelled_captures.saturating_sub(1);
        }
        state.last_activity = Instant::now();
        let permit = (matches!(state.phase, Phase::Ending(_)) && state.uncancelled_captures == 0)
            .then(|| state.holdings.permit.take())
            .flatten();
        drop(state);
        drop(permit);
        self.changed.notify_one();
    }

    fn acquire_returned(&self) {
        self.state().acquiring = false;
        self.changed.notify_one();
    }

    /// Nothing runs under the lease, and its acquire has returned.
    fn quiet(&self) -> bool {
        let state = self.state();
        state.calls == 0 && !state.acquiring
    }
}

/// Marks the acquire's body as returned when dropped, so the keeper may
/// give back what it took.
#[must_use = "dropping it tells the keeper the acquire has returned"]
pub struct Acquiring(Arc<Lease>);

impl Acquiring {
    #[must_use]
    pub const fn lease(&self) -> &Arc<Lease> {
        &self.0
    }
}

impl Drop for Acquiring {
    fn drop(&mut self) {
        // An acquire that returns without its grant, a body that
        // panicked or was dropped included, ends its lease, so the
        // reservation never outlives it.
        let ungranted = matches!(self.0.state().phase, Phase::Acquiring);
        if ungranted {
            self.0.end(LeaseEnd::AcquireFailed);
        }
        self.0.acquire_returned();
    }
}

/// One call under a lease. Dropping it ends the call.
#[must_use = "dropping it ends the call under the lease"]
pub struct LeaseCall {
    lease: Arc<Lease>,
    device: String,
    capture: bool,
    cancelled: bool,
}

impl LeaseCall {
    #[must_use]
    pub fn lease_id(&self) -> &str {
        &self.lease.id
    }

    /// The lease's slot for this call's camera or device: calls under
    /// one lease through one of them run one at a time.
    pub async fn slot(&self) -> SlotGuard {
        match self.lease.inner_slots.get(&self.device) {
            Some(slot) => slot.take("leased device", &self.device).await,
            // `admit` let the call through only for a camera or device in
            // the plan, and the lease made a slot for each.
            None => {
                DeviceSlot::default()
                    .take("leased device", &self.device)
                    .await
            }
        }
    }

    /// The capture this call runs was cancelled mid-exposure: it no
    /// longer keeps an ending lease's gate permit.
    pub fn mark_cancelled(&mut self) {
        if self.capture && !self.cancelled {
            self.cancelled = true;
            self.lease.capture_cancelled();
        }
    }
}

impl Drop for LeaseCall {
    fn drop(&mut self) {
        self.lease.call_ended(self.capture && !self.cancelled);
    }
}

enum Seen {
    Live(Arc<Lease>),
    Ended { end: LeaseEnd, at: Instant },
    ReleasedFirst { at: Instant },
}

impl Seen {
    const fn remembered_since(&self) -> Option<Instant> {
        match self {
            Self::Live(_) => None,
            Self::Ended { at, .. } | Self::ReleasedFirst { at } => Some(*at),
        }
    }

    /// Why an id that holds no live lease is not held.
    fn why_not_held(&self) -> String {
        match self {
            Self::Live(_) => "it is live".to_string(),
            Self::Ended { end, .. } => end.why(),
            Self::ReleasedFirst { .. } => "a release named it before any acquire".to_string(),
        }
    }

    fn history(&self) -> String {
        match self {
            Self::Live(lease) => match &lease.state().phase {
                Phase::Acquiring => "its acquire is still waiting".to_string(),
                Phase::Granted { .. } => "it holds a lease".to_string(),
                Phase::Ending(end) => format!("its lease ended: {}", end.why()),
            },
            Self::Ended { end, .. } => format!("its lease ended: {}", end.why()),
            Self::ReleasedFirst { .. } => "a release named it before any acquire".to_string(),
        }
    }
}

struct Sweep {
    label: String,
    camera_id: String,
    focuser_id: String,
}

#[derive(Default)]
struct Book {
    seen: HashMap<String, Seen>,
    sweeps: HashMap<u64, Sweep>,
    next_sweep: u64,
}

impl Book {
    fn forget_old(&mut self, now: Instant) {
        self.seen.retain(|_, seen| {
            seen.remembered_since()
                .is_none_or(|at| now < later(at, SEEN_ID_MEMORY))
        });
    }

    fn live(&self) -> impl Iterator<Item = &Arc<Lease>> {
        self.seen.values().filter_map(|seen| match seen {
            Seen::Live(lease) => Some(lease),
            Seen::Ended { .. } | Seen::ReleasedFirst { .. } => None,
        })
    }

    /// The refusal of a new id at the cap, naming when the oldest
    /// remembered id is forgotten.
    fn at_cap(&self, now: Instant) -> String {
        let oldest = self
            .seen
            .values()
            .filter_map(Seen::remembered_since)
            .min()
            .map_or_else(
                || "an hour after the oldest lease ends".to_string(),
                |at| {
                    let left = later(at, SEEN_ID_MEMORY).saturating_duration_since(now);
                    format!("in {} s", left.as_secs())
                },
            );
        format!(
            "rp remembers {SEEN_ID_CAP} lease ids, the most it keeps; the oldest is forgotten {oldest}"
        )
    }
}

/// The book of train leases, shared by the MCP handler, the safety
/// enforcer and the server's shutdown.
#[derive(Default)]
pub struct TrainLeases {
    book: Mutex<Book>,
}

impl TrainLeases {
    fn lock(&self) -> MutexGuard<'_, Book> {
        // A poisoned lock only means a holder panicked; the book is
        // still consistent.
        self.book.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Record `lease_id` and its reservation, and start the lease's
    /// keeper. The acquire's waits follow; dropping the returned
    /// [`Acquiring`] tells the keeper they are over.
    ///
    /// # Errors
    ///
    /// An id seen before, a new id at the cap, or a reservation that
    /// overlaps another lease's or a running sweep's.
    pub fn reserve(self: &Arc<Self>, lease_id: &str, plan: LeasePlan) -> Result<Acquiring, String> {
        let now = Instant::now();
        let mut book = self.lock();
        book.forget_old(now);
        if let Some(seen) = book.seen.get(lease_id) {
            return Err(format!(
                "lease id '{lease_id}' was used before ({}); name a fresh id",
                seen.history()
            ));
        }
        if book.seen.len() >= SEEN_ID_CAP {
            return Err(book.at_cap(now));
        }
        if let Some((kind, id, other)) = book.live().find_map(|other| {
            plan.shared_with(&other.plan)
                .map(|(kind, id)| (kind, id, other))
        }) {
            return Err(format!(
                "train '{}' shares {kind} '{id}' with lease '{}' on train '{}'",
                plan.train_id, other.id, other.plan.train_id
            ));
        }
        if let Some(sweep) = book
            .sweeps
            .values()
            .find(|s| plan.reserves(&s.camera_id) || plan.reserves(&s.focuser_id))
        {
            return Err(format!(
                "train '{}' overlaps {}, which is running",
                plan.train_id, sweep.label
            ));
        }
        let lease = Arc::new(Lease::new(lease_id, plan));
        book.seen
            .insert(lease_id.to_string(), Seen::Live(lease.clone()));
        drop(book);
        debug!(lease_id, train_id = %lease.plan.train_id, "train lease reserved");
        tokio::spawn(self.clone().keep(lease.clone()));
        Ok(Acquiring(lease))
    }

    /// End the lease `lease_id` names, granted or still being acquired.
    /// `true` when this call ended it. An id rp has not seen is recorded,
    /// below the cap, so an acquire that arrives after the release is
    /// refused.
    pub fn release(&self, lease_id: &str) -> bool {
        let now = Instant::now();
        let mut book = self.lock();
        book.forget_old(now);
        match book.seen.get(lease_id) {
            Some(Seen::Live(lease)) => {
                let lease = lease.clone();
                drop(book);
                lease.end(LeaseEnd::Released)
            }
            Some(Seen::Ended { .. } | Seen::ReleasedFirst { .. }) => false,
            None if book.seen.len() < SEEN_ID_CAP => {
                book.seen
                    .insert(lease_id.to_string(), Seen::ReleasedFirst { at: now });
                debug!(lease_id, "release of an unseen lease id recorded");
                false
            }
            None => {
                debug!(
                    lease_id,
                    "release of an unseen lease id at the cap; not recorded"
                );
                false
            }
        }
    }

    /// Admit one call under `lease_id`.
    ///
    /// # Errors
    ///
    /// Why the lease is not held, or why the call may not run under it.
    pub fn admit(&self, lease_id: &str, call: CallKind<'_>) -> Result<LeaseCall, String> {
        let lease = {
            let mut book = self.lock();
            book.forget_old(Instant::now());
            match book.seen.get(lease_id) {
                Some(Seen::Live(lease)) => lease.clone(),
                Some(seen) => return Err(not_held(lease_id, &seen.why_not_held())),
                None => {
                    return Err(not_held(
                        lease_id,
                        "rp has not granted it (a restart forgets every lease)",
                    ))
                }
            }
        };
        lease.admit(call)
    }

    /// The live lease whose reservation includes any of `ids`, with the
    /// token fired once it has given everything back: what a call that
    /// names no lease waits for.
    #[must_use]
    pub fn reserving(&self, ids: &[&str]) -> Option<(String, CancellationToken)> {
        self.lock()
            .live()
            .find(|lease| ids.iter().any(|id| lease.plan.reserves(id)))
            .map(|lease| (lease.id.clone(), lease.released.clone()))
    }

    /// Enter one of rp's own sweeps in the reservation table for its
    /// length: its camera and its focuser.
    ///
    /// # Errors
    ///
    /// A live lease that reserves either.
    pub fn enter_sweep(
        self: &Arc<Self>,
        label: String,
        camera_id: &str,
        focuser_id: &str,
    ) -> Result<SweepGuard, String> {
        let mut book = self.lock();
        let conflict = book.live().find_map(|lease| {
            [("camera", camera_id), ("focuser", focuser_id)]
                .into_iter()
                .find(|(_, id)| lease.plan.reserves(id))
                .map(|(kind, id)| (kind, id, lease.clone()))
        });
        if let Some((kind, id, lease)) = conflict {
            return Err(format!(
                "{label} is refused: lease '{}' on train '{}' reserves {kind} '{id}'",
                lease.id, lease.plan.train_id
            ));
        }
        let key = book.next_sweep;
        book.next_sweep = book.next_sweep.wrapping_add(1);
        book.sweeps.insert(
            key,
            Sweep {
                label,
                camera_id: camera_id.to_string(),
                focuser_id: focuser_id.to_string(),
            },
        );
        Ok(SweepGuard {
            leases: self.clone(),
            key,
        })
    }

    /// End every live lease (the safety transition, shutdown).
    pub fn end_all(&self, end: &LeaseEnd) {
        let live: Vec<Arc<Lease>> = self.lock().live().cloned().collect();
        for lease in live {
            lease.end(end.clone());
        }
    }

    /// Wait, at most `bound`, for every live lease to give everything
    /// back.
    pub async fn settle(&self, bound: Duration) {
        let released: Vec<CancellationToken> = self
            .lock()
            .live()
            .map(|lease| lease.released.clone())
            .collect();
        let all = async {
            for token in released {
                token.cancelled().await;
            }
        };
        if tokio::time::timeout(bound, all).await.is_err() {
            debug!(
                ?bound,
                "train leases still giving back at the shutdown bound"
            );
        }
    }

    /// The keeper: renew the facade lease, end the lease idle or at its
    /// `max_duration`, then give everything back once it has ended and
    /// nothing runs under it.
    async fn keep(self: Arc<Self>, lease: Arc<Lease>) {
        loop {
            let (end_at, renew_at) = lease.next_wake();
            tokio::select! {
                biased;
                () = lease.ending.cancelled() => break,
                () = lease.changed.notified() => {}
                () = sleep_until(end_at) => lease.end_if_due(Instant::now()),
                () = sleep_until(renew_at) => {
                    if let Some((camera, token)) = lease.due_renewal(Instant::now()) {
                        match facade::renew(&camera, &token).await {
                            facade::Renewal::Renewed => {
                                debug!(lease_id = %lease.id, "renewed the camera facade's sweep lease");
                            }
                            facade::Renewal::Lost(reason) => {
                                lease.end(LeaseEnd::FacadeLost(reason));
                            }
                            facade::Renewal::Unreached(reason) => {
                                debug!(lease_id = %lease.id, %reason, "a renewal of the camera facade's sweep lease did not reach it; trying again at the next tick");
                            }
                        }
                    }
                }
            }
        }
        while !lease.quiet() {
            lease.changed.notified().await;
        }
        let facade = lease.state().facade.take();
        if let Some(facade) = facade {
            facade::release(&facade.camera, &facade.token).await;
        }
        let holdings = std::mem::take(&mut lease.state().holdings);
        drop(holdings);
        let end = lease.end_reason().unwrap_or(LeaseEnd::AcquireFailed);
        self.lock().seen.insert(
            lease.id.clone(),
            Seen::Ended {
                end,
                at: Instant::now(),
            },
        );
        lease.released.cancel();
        debug!(lease_id = %lease.id, "train lease gave everything back");
    }
}

/// `at` plus `by`. The durations here are minutes, far from the clock's
/// limit; an overflow would read as `at` itself.
fn later(at: Instant, by: Duration) -> Instant {
    at.checked_add(by).unwrap_or(at)
}

async fn sleep_until(at: Option<Instant>) {
    match at {
        Some(at) => tokio::time::sleep_until(at).await,
        None => std::future::pending().await,
    }
}

/// Keeps one of rp's sweeps in the reservation table; dropping it takes
/// the sweep out.
#[must_use = "dropping it takes the sweep out of the reservation table"]
pub struct SweepGuard {
    leases: Arc<TrainLeases>,
    key: u64,
}

impl Drop for SweepGuard {
    fn drop(&mut self) {
        self.leases.lock().sweeps.remove(&self.key);
    }
}

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
mod tests {
    use std::sync::atomic::Ordering;

    use tokio::sync::RwLock;

    use super::test_support::FakeFacade;
    use super::*;

    /// The reference rig of rp.md § Optical Trains, as `pier1` is: the
    /// EAF shared by both trains, the wheel and the rotator in the main
    /// train only, the guide train's own helical behind the EAF.
    fn reference_trains() -> TrainModel {
        let equipment: crate::config::EquipmentConfig = serde_json::from_value(serde_json::json!({
            "cameras": [
                {"id": "main-cam", "alpaca_url": "http://x"},
                {"id": "guide-cam", "alpaca_url": "http://x"}
            ],
            "focusers": [
                {"id": "eaf", "alpaca_url": "http://x"},
                {"id": "scops-focuser", "alpaca_url": "http://x"}
            ],
            "rotators": [{"id": "falcon", "alpaca_url": "http://x"}],
            "filter_wheels": [{"id": "main-fw", "alpaca_url": "http://x"}],
            "mount": {"alpaca_url": "http://x", "guiding": {"url": "http://x"}},
            "optical_trains": [
                {"id": "main", "purpose": "imaging",
                 "devices": ["eaf", "main-fw", "falcon", "main-cam"]},
                {"id": "guide", "purpose": "guiding",
                 "devices": ["eaf", "scops-focuser", "guide-cam"]}
            ]
        }))
        .unwrap();
        TrainModel::try_from_equipment(&equipment).unwrap()
    }

    fn plan(train_id: &str) -> LeasePlan {
        LeasePlan::for_train(&reference_trains(), train_id).unwrap()
    }

    /// A lease on `train_id`, granted for `life`, its acquire returned.
    fn granted(leases: &Arc<TrainLeases>, id: &str, train_id: &str, life: Duration) -> Arc<Lease> {
        let acquiring = leases.reserve(id, plan(train_id)).unwrap();
        acquiring.lease().grant(life).unwrap();
        acquiring.lease().clone()
    }

    const fn capture_through(camera_id: &str) -> CallKind<'_> {
        CallKind::Capture { camera_id }
    }

    #[test]
    fn a_guiding_trains_lease_moves_its_own_focuser_and_holds_the_shared_one_still() {
        let guide = plan("guide");
        assert!(guide.guiding);
        assert_eq!(guide.camera_id, "guide-cam");
        assert_eq!(guide.focuser_id, "scops-focuser");
        assert_eq!(guide.filter_wheel_ids, Vec::<String>::new());
        assert_eq!(guide.held_focuser_ids, ["eaf"]);
        assert_eq!(guide.held_rotator_ids, Vec::<String>::new());
        // Nothing behind the main camera moves: the guide helical is in
        // the guide train only.
        assert_eq!(guide.drained_camera_ids, Vec::<String>::new());
    }

    #[test]
    fn an_imaging_trains_lease_holds_every_camera_behind_what_it_moves() {
        let main = plan("main");
        assert!(!main.guiding);
        assert_eq!(main.focuser_id, "eaf");
        assert_eq!(main.filter_wheel_ids, ["main-fw"]);
        assert_eq!(main.held_focuser_ids, Vec::<String>::new());
        assert_eq!(main.held_rotator_ids, ["falcon"]);
        // The EAF moves the guide camera's focus too.
        assert_eq!(main.drained_camera_ids, ["guide-cam"]);
        assert_eq!(
            main.cameras().collect::<Vec<_>>(),
            ["main-cam", "guide-cam"]
        );
    }

    #[test]
    fn a_train_without_a_focuser_has_no_lease() {
        let equipment: crate::config::EquipmentConfig = serde_json::from_value(serde_json::json!({
            "cameras": [{"id": "solo-cam", "alpaca_url": "http://x"}],
            "optical_trains": [{"id": "solo", "devices": ["solo-cam"]}]
        }))
        .unwrap();
        let trains = TrainModel::try_from_equipment(&equipment).unwrap();
        let err = LeasePlan::for_train(&trains, "solo").unwrap_err();
        assert_eq!(err, "train 'solo' has no focuser");
        let err = LeasePlan::for_train(&trains, "nope").unwrap_err();
        assert_eq!(err, "train not found: nope");
    }

    #[test]
    fn a_lease_id_takes_printable_ascii_without_spaces() {
        check_lease_id("3f6c2a51-9b7e-4d08-a3c4-5e1f8b2d7c90").unwrap();
        for bad in ["", "a b", "tab\there", "naïve", &"x".repeat(129)] {
            check_lease_id(bad).unwrap_err();
        }
    }

    #[tokio::test]
    async fn an_id_is_never_reserved_twice() {
        let leases = Arc::new(TrainLeases::default());
        drop(granted(&leases, "a", "main", Duration::from_mins(5)));
        assert!(leases.release("a"));

        let Err(err) = leases.reserve("a", plan("main")) else {
            panic!("a reused id was reserved");
        };
        assert!(err.contains("lease id 'a' was used before"), "{err}");
    }

    #[tokio::test]
    async fn a_release_that_comes_first_turns_its_acquire_away() {
        let leases = Arc::new(TrainLeases::default());
        assert!(!leases.release("a"));

        let Err(err) = leases.reserve("a", plan("main")) else {
            panic!("an id a release named was reserved");
        };
        assert!(
            err.contains("a release named it before any acquire"),
            "{err}"
        );
    }

    #[tokio::test]
    async fn a_reservation_sharing_a_camera_or_device_with_a_live_lease_is_refused() {
        let leases = Arc::new(TrainLeases::default());
        let _main = granted(&leases, "a", "main", Duration::from_mins(5));

        let Err(err) = leases.reserve("b", plan("guide")) else {
            panic!("two leases share the guide camera and the EAF");
        };
        assert_eq!(
            err,
            "train 'guide' shares camera 'guide-cam' with lease 'a' on train 'main'"
        );
    }

    #[tokio::test]
    async fn a_running_sweep_and_a_lease_refuse_each_other() {
        let leases = Arc::new(TrainLeases::default());
        let sweep = leases
            .enter_sweep("the test sweep".to_string(), "main-cam", "eaf")
            .unwrap();
        let Err(err) = leases.reserve("a", plan("main")) else {
            panic!("a lease was reserved over a running sweep");
        };
        assert_eq!(
            err,
            "train 'main' overlaps the test sweep, which is running"
        );
        drop(sweep);

        let _lease = granted(&leases, "b", "guide", Duration::from_mins(5));
        let Err(err) = leases.enter_sweep("the main sweep".to_string(), "main-cam", "eaf") else {
            panic!("a sweep entered over a held focuser");
        };
        assert_eq!(
            err,
            "the main sweep is refused: lease 'b' on train 'guide' reserves focuser 'eaf'"
        );
    }

    #[tokio::test]
    async fn two_sweeps_never_refuse_each_other() {
        let leases = Arc::new(TrainLeases::default());
        let _first = leases.enter_sweep("one".into(), "main-cam", "eaf").unwrap();
        let _second = leases.enter_sweep("two".into(), "main-cam", "eaf").unwrap();
    }

    #[tokio::test]
    async fn a_call_is_admitted_only_between_the_grant_and_the_end() {
        let leases = Arc::new(TrainLeases::default());
        let err = leases
            .admit("a", capture_through("main-cam"))
            .err()
            .unwrap();
        assert_eq!(
            err,
            "lease 'a' is not held: rp has not granted it (a restart forgets every lease)"
        );

        let acquiring = leases.reserve("a", plan("main")).unwrap();
        let err = leases
            .admit("a", capture_through("main-cam"))
            .err()
            .unwrap();
        assert_eq!(err, "lease 'a' is not held: its acquire is still waiting");

        acquiring.lease().grant(Duration::from_mins(5)).unwrap();
        drop(leases.admit("a", capture_through("main-cam")).unwrap());

        leases.release("a");
        let err = leases
            .admit("a", capture_through("main-cam"))
            .err()
            .unwrap();
        assert_eq!(err, "lease 'a' is not held: it was released");
    }

    #[tokio::test]
    async fn a_call_under_the_lease_names_what_it_may_not_touch() {
        let leases = Arc::new(TrainLeases::default());
        let _lease = granted(&leases, "a", "guide", Duration::from_mins(5));

        let err = leases
            .admit("a", capture_through("main-cam"))
            .err()
            .unwrap();
        assert_eq!(err, "camera 'main-cam' is not held by lease 'a'");
        let err = leases
            .admit(
                "a",
                CallKind::Move {
                    kind: "focuser",
                    device_id: "eaf",
                },
            )
            .err()
            .unwrap();
        assert_eq!(
            err,
            "lease 'a' holds focuser 'eaf' still; a call under it cannot move it"
        );
        let err = leases
            .admit(
                "a",
                CallKind::Move {
                    kind: "filter wheel",
                    device_id: "main-fw",
                },
            )
            .err()
            .unwrap();
        assert_eq!(err, "filter wheel 'main-fw' is not in lease 'a'");
        drop(
            leases
                .admit(
                    "a",
                    CallKind::Move {
                        kind: "focuser",
                        device_id: "scops-focuser",
                    },
                )
                .unwrap(),
        );
    }

    #[tokio::test]
    async fn a_reserved_camera_or_device_names_its_lease_until_the_lease_gives_back() {
        let leases = Arc::new(TrainLeases::default());
        let lease = granted(&leases, "a", "guide", Duration::from_mins(5));
        assert!(leases.reserving(&["main-cam"]).is_none());
        let (id, released) = leases.reserving(&["eaf"]).unwrap();
        assert_eq!(id, "a");

        leases.release("a");
        tokio::time::timeout(Duration::from_secs(5), released.cancelled())
            .await
            .expect("an ended lease with nothing under it gives back at once");
        assert!(leases.reserving(&["eaf"]).is_none());
        assert_eq!(lease.end_reason(), Some(LeaseEnd::Released));
    }

    #[tokio::test(start_paused = true)]
    async fn a_granted_lease_ends_a_minute_after_its_last_call() {
        let leases = Arc::new(TrainLeases::default());
        let lease = granted(&leases, "a", "main", Duration::from_mins(5));

        tokio::time::sleep(Duration::from_secs(50)).await;
        let call = leases.admit("a", capture_through("main-cam")).unwrap();
        // A call in flight holds the idle timeout off however long it runs.
        tokio::time::sleep(Duration::from_secs(90)).await;
        assert_eq!(lease.end_reason(), None);
        drop(call);

        tokio::time::sleep(Duration::from_secs(59)).await;
        assert_eq!(lease.end_reason(), None, "ended before a minute had passed");
        tokio::time::sleep(Duration::from_secs(2)).await;
        assert_eq!(lease.end_reason(), Some(LeaseEnd::Idle));
        let err = leases
            .admit("a", capture_through("main-cam"))
            .err()
            .unwrap();
        assert_eq!(
            err,
            "lease 'a' is not held: it ended after 60 s with no call under it"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn a_lease_ends_at_its_max_duration_even_with_a_call_in_flight() {
        let leases = Arc::new(TrainLeases::default());
        let lease = granted(&leases, "a", "main", Duration::from_mins(2));
        let call = leases.admit("a", capture_through("main-cam")).unwrap();

        tokio::time::sleep(Duration::from_secs(121)).await;
        assert_eq!(lease.end_reason(), Some(LeaseEnd::MaxDuration));
        let err = leases
            .admit("a", capture_through("main-cam"))
            .err()
            .unwrap();
        assert_eq!(err, "lease 'a' is not held: it reached its max_duration");
        drop(call);
    }

    /// An exclusive acquire of `gate` succeeds at once: nothing holds it
    /// shared.
    fn gate_free(gate: &Arc<RwLock<()>>) -> bool {
        gate.try_write().is_ok()
    }

    #[tokio::test]
    async fn an_ending_lease_keeps_the_gate_while_a_capture_under_it_runs() {
        let leases = Arc::new(TrainLeases::default());
        let lease = granted(&leases, "a", "main", Duration::from_mins(5));
        let gate = Arc::new(RwLock::new(()));
        lease.hold_permit(gate.clone().read_owned().await);
        let capture = leases.admit("a", capture_through("main-cam")).unwrap();

        leases.release("a");
        assert!(
            !gate_free(&gate),
            "the gate went back under a running capture"
        );
        drop(capture);
        assert!(
            gate_free(&gate),
            "the gate stayed held after the last capture"
        );
    }

    #[tokio::test]
    async fn a_cancelled_capture_gives_an_ending_leases_gate_back_at_once() {
        let leases = Arc::new(TrainLeases::default());
        let lease = granted(&leases, "a", "main", Duration::from_mins(5));
        let gate = Arc::new(RwLock::new(()));
        lease.hold_permit(gate.clone().read_owned().await);
        let mut capture = leases.admit("a", capture_through("main-cam")).unwrap();

        capture.mark_cancelled();
        assert!(!gate_free(&gate), "a live lease keeps its gate");
        leases.release("a");
        assert!(
            gate_free(&gate),
            "an ending lease kept the gate for a cancelled capture"
        );
        drop(capture);
    }

    #[tokio::test]
    async fn an_ending_lease_keeps_its_slots_until_its_last_call_ends() {
        let leases = Arc::new(TrainLeases::default());
        let lease = granted(&leases, "a", "main", Duration::from_mins(5));
        let slot = DeviceSlot::default();
        lease.hold(slot.take("focuser", "eaf").await);
        let call = leases
            .admit(
                "a",
                CallKind::Move {
                    kind: "focuser",
                    device_id: "eaf",
                },
            )
            .unwrap();

        leases.release("a");
        let early = tokio::time::timeout(Duration::from_millis(100), slot.take("focuser", "eaf"));
        assert!(
            early.await.is_err(),
            "the slot went back under a running move"
        );
        drop(call);
        tokio::time::timeout(Duration::from_secs(5), slot.take("focuser", "eaf"))
            .await
            .expect("the slot stayed held after the last call");
    }

    #[tokio::test]
    async fn an_acquire_that_returns_ungranted_ends_its_lease() {
        let leases = Arc::new(TrainLeases::default());
        let acquiring = leases.reserve("a", plan("main")).unwrap();
        let lease = acquiring.lease().clone();
        let (_, released) = leases.reserving(&["eaf"]).unwrap();

        // What a panicking or dropped acquire body leaves behind.
        drop(acquiring);

        assert_eq!(lease.end_reason(), Some(LeaseEnd::AcquireFailed));
        tokio::time::timeout(Duration::from_secs(5), released.cancelled())
            .await
            .expect("an ungranted lease kept its reservation");
        assert!(leases.reserving(&["eaf"]).is_none());
    }

    #[tokio::test]
    async fn an_acquire_ended_while_it_waits_keeps_what_it_took_until_it_returns() {
        let leases = Arc::new(TrainLeases::default());
        let acquiring = leases.reserve("a", plan("main")).unwrap();
        let slot = DeviceSlot::default();
        assert!(
            leases.release("a"),
            "a release ends an acquire still waiting"
        );

        // A slot the acquire took as the release landed still goes back.
        acquiring.lease().hold(slot.take("focuser", "eaf").await);
        let (_, released) = leases.reserving(&["eaf"]).unwrap();
        let early = tokio::time::timeout(Duration::from_millis(100), released.cancelled());
        assert!(
            early.await.is_err(),
            "the lease gave back before its acquire returned"
        );
        drop(acquiring);
        tokio::time::timeout(Duration::from_secs(5), slot.take("focuser", "eaf"))
            .await
            .expect("the acquire's slot was never given back");
    }

    #[tokio::test(start_paused = true)]
    async fn a_remembered_id_is_forgotten_an_hour_after_it_was_recorded() {
        let leases = Arc::new(TrainLeases::default());
        leases.release("a");
        tokio::time::sleep(Duration::from_secs(3599)).await;
        assert!(leases.reserve("a", plan("main")).is_err());
        tokio::time::sleep(Duration::from_secs(2)).await;
        drop(leases.reserve("a", plan("main")).unwrap());
    }

    #[tokio::test(start_paused = true)]
    async fn at_the_cap_a_new_id_is_refused_and_an_unseen_release_is_not_recorded() {
        let leases = Arc::new(TrainLeases::default());
        for n in 0..SEEN_ID_CAP {
            leases.release(&format!("id-{n}"));
        }
        let Err(err) = leases.reserve("new", plan("main")) else {
            panic!("a new id was reserved at the cap");
        };
        assert!(
            err.contains("rp remembers 4096 lease ids") && err.contains("forgotten in 3600 s"),
            "{err}"
        );

        assert!(!leases.release("late"), "a release is never refused");
        tokio::time::sleep(SEEN_ID_MEMORY).await;
        // The cap's ids are forgotten, and the release at the cap left no
        // record behind, so the late acquire is granted.
        drop(leases.reserve("late", plan("main")).unwrap());
    }

    /// A guiding train's granted lease whose facade lease `facade`
    /// granted under `token`.
    fn with_facade(leases: &Arc<TrainLeases>, facade: &Arc<FakeFacade>, token: &str) -> Arc<Lease> {
        let acquiring = leases.reserve("g", plan("guide")).unwrap();
        let camera: Arc<dyn Camera> = facade.clone();
        acquiring.lease().record_facade(camera, token.to_string());
        acquiring.lease().facade_granted();
        acquiring.lease().grant(Duration::from_mins(5)).unwrap();
        acquiring.lease().clone()
    }

    #[tokio::test(start_paused = true)]
    async fn the_facade_lease_is_renewed_every_20_s_and_released_with_the_lease() {
        let leases = Arc::new(TrainLeases::default());
        let facade = Arc::new(FakeFacade::default());
        let lease = with_facade(&leases, &facade, "tok");
        // A call in flight keeps the idle timeout out of the way.
        let call = leases.admit("g", capture_through("guide-cam")).unwrap();

        tokio::time::sleep(Duration::from_millis(19_900)).await;
        assert!(facade.calls_of(facade::RENEW).is_empty(), "renewed early");
        tokio::time::sleep(Duration::from_millis(200)).await;
        assert_eq!(facade.calls_of(facade::RENEW).len(), 1);
        tokio::time::sleep(Duration::from_secs(40)).await;
        assert_eq!(facade.calls_of(facade::RENEW).len(), 3);
        assert_eq!(facade.calls_of(facade::RENEW)[0]["token"], "tok");

        drop(call);
        leases.release("g");
        tokio::time::timeout(Duration::from_secs(5), lease.released.cancelled())
            .await
            .unwrap();
        let releases = facade.calls_of(facade::RELEASE);
        assert_eq!(releases.len(), 1);
        assert_eq!(releases[0]["token"], "tok");
    }

    #[tokio::test(start_paused = true)]
    async fn a_refused_renewal_ends_the_lease_and_an_unreached_one_does_not() {
        let leases = Arc::new(TrainLeases::default());
        let facade = Arc::new(FakeFacade::default());
        let lease = with_facade(&leases, &facade, "tok");
        let call = leases.admit("g", capture_through("guide-cam")).unwrap();

        facade.unreachable.store(true, Ordering::SeqCst);
        tokio::time::sleep(Duration::from_secs(21)).await;
        assert_eq!(
            lease.end_reason(),
            None,
            "an unreached renewal ended the lease"
        );

        facade.unreachable.store(false, Ordering::SeqCst);
        facade.lost.store(true, Ordering::SeqCst);
        tokio::time::sleep(Duration::from_secs(20)).await;
        let Some(LeaseEnd::FacadeLost(reason)) = lease.end_reason() else {
            panic!(
                "a refused renewal left the lease live: {:?}",
                lease.end_reason()
            );
        };
        assert!(reason.contains("ended by a guiding stop"), "{reason}");
        let err = leases
            .admit("g", capture_through("guide-cam"))
            .err()
            .unwrap();
        assert!(
            err.starts_with("lease 'g' is not held: it lost the camera facade's sweep lease"),
            "{err}"
        );
        drop(call);
    }

    #[tokio::test]
    async fn end_all_ends_every_live_lease_with_its_reason() {
        let leases = Arc::new(TrainLeases::default());
        let main = granted(&leases, "a", "main", Duration::from_mins(5));
        leases.end_all(&LeaseEnd::Safety);
        assert_eq!(main.end_reason(), Some(LeaseEnd::Safety));
        leases.settle(Duration::from_secs(5)).await;
        let err = leases
            .admit("a", capture_through("main-cam"))
            .err()
            .unwrap();
        assert_eq!(err, "lease 'a' is not held: the safety transition ended it");
    }
}
