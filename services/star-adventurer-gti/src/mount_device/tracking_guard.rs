//! Tracking-time watcher: CW-exclusion-zone safety guard + opt-in
//! auto-flip.
//!
//! The slew planner ([`super::slew`]) keeps the counterweights clear of
//! the CW exclusion zone at *slew* time, but once `Tracking = true` the
//! firmware advances the RA encoder autonomously and the driver issues
//! no further wire commands. A multi-hour imaging session that starts
//! at a safe negative `mech_HA` will drift up across the zone entry
//! (`mech_HA = +0.95` by default) with no intervention — the same
//! physical failure mode as a zone-crossing slew, just reached via
//! tracking drift. See issue #259.
//!
//! This module closes that gap with a per-connection background task
//! that watches the live encoder `mech_HA` while tracking. Two actions
//! hang off the same tick:
//!
//! - **Safety guard** (always on while the zone is active, independent
//!   of [`crate::config::FlipPolicy::enabled`]): stop the mount (`:K1`)
//!   before it can drift into the zone, clear the in-memory `Tracking`
//!   flag to match, and emit a `warn!`. The guard does **not** pick a
//!   pier side or flip; the operator (or higher-level automation)
//!   decides what to do next.
//! - **Auto-flip** (opt-in via
//!   [`crate::config::FlipPolicy::auto_flip_during_tracking`], under
//!   the `enabled` master switch): once `mech_HA` reaches the
//!   configured meridian offset on the natural pier side, issue the
//!   same through-wrap flip slew an explicit `SetSideOfPier` would.
//!   Tracking re-engages on the new pier side via the standard
//!   slew-completion watcher. One attempt per meridian crossing; the
//!   stop-only guard stays the fallback when an attempt fails. See the
//!   design doc's
//!   [§"Auto-flip during tracking"](../../../../docs/services/star-adventurer-gti.md#auto-flip-during-tracking).
//!
//! The watcher reads the snapshot the background poll loop already
//! refreshes (it does not poll the wire itself), so it is the
//! "extension of the existing poll loop" issue #259 describes without
//! crossing into the transport layer.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use ascom_alpaca::api::telescope::Telescope;
use rusty_photon_shared_transport::Session;
use skywatcher_motor_protocol::{Axis, Command};
use tokio::sync::RwLock;
use tokio::time::interval;
use tracing::{debug, info, warn};

use crate::codec::SkywatcherCodec;
use crate::coordinates::{opposite_pier_side, side_of_pier as side_of_pier_calc};
use crate::manager::MountManager;
use crate::units::{Cpr, DecTicks, RaTicks};

use super::{pre_flip_side_for_latitude, DriverState, MountDevice};

/// Device session slot, shared with the completion watchers. `Some`
/// between `set_connected(true)` and `set_connected(false)`.
type SessionSlot = Arc<RwLock<Option<Session<SkywatcherCodec>>>>;

/// Does the instantaneous encoder `mech_HA` fall inside the guarded
/// band — the CW exclusion zone `(zone_min, zone_max)` widened by
/// `margin` on both edges, i.e. the open interval
/// `(zone_min − margin, zone_max + margin)`?
///
/// The check is on a single folded `mech_HA` value (already in
/// `[−12, +12)`), not a swept path, so unlike
/// [`super::slew::canonical_path_crosses_binding_zone`] it needs no
/// 24-hour-wrap handling: the realistic zone+margin stays well within
/// the folded range.
///
/// An empty/inverted interval (`zone_min >= zone_max`) disables the
/// guard — the same convention the slew-path binding-zone check uses.
/// A non-finite or negative `margin` is treated as `0.0` (stop exactly
/// at zone entry) so a bad value fails safe rather than open. Config-file
/// margins are already rejected at load — [`crate::config::TrackingGuardMarginHours`]
/// validates at deserialize time; this sanitisation is
/// defense-in-depth for construction paths that bypass that check
/// (programmatic configs, tests).
pub(super) fn tracking_guard_breached(mech_ha: f64, zone: (f64, f64), margin: f64) -> bool {
    let (zone_min, zone_max) = zone;
    if zone_min >= zone_max {
        return false;
    }
    let margin = if margin.is_finite() && margin >= 0.0 {
        margin
    } else {
        0.0
    };
    mech_ha > zone_min - margin && mech_ha < zone_max + margin
}

/// One guard evaluation against the latest cached snapshot.
///
/// Returns `true` only when it stopped tracking. It is a no-op (returns
/// `false`) when the client has not engaged tracking, a slew is in
/// flight, the zone is disabled, the snapshot `mech_HA` is clear of the
/// band, parameters aren't cached yet, or the session closed mid-tick.
pub(super) async fn tracking_guard_tick(
    state: &Arc<RwLock<DriverState>>,
    manager: &MountManager,
    session_slot: &SessionSlot,
    slew_in_progress: &AtomicBool,
    zone: (f64, f64),
    margin: f64,
) -> bool {
    // Cheap gate first: only intervene while the client has tracking
    // engaged and no slew is in flight. `slew_to_coordinates_async`
    // clears `tracking_requested` for the slew's duration, so gating on
    // it already keeps the guard dormant during slews; the
    // `slew_in_progress` check is belt-and-suspenders for the brief
    // post-slew tracking-restart window.
    if !state.read().await.tracking_requested || slew_in_progress.load(Ordering::SeqCst) {
        return false;
    }
    // `mech_HA` needs the per-axis CPR captured at handshake.
    let Some(params) = manager.parameters().await else {
        return false;
    };
    let snap = manager.snapshot().await;
    let mech_ha = RaTicks::new(snap.ra.position_ticks)
        .to_mech_ha(Cpr::new(params.cpr_ra))
        .value();
    if !tracking_guard_breached(mech_ha, zone, margin) {
        return false;
    }

    // Breached. Stop RA tracking on the wire FIRST, mirroring
    // `set_tracking(false)` (`:K1`, then clear the flag), so the
    // in-memory `Tracking` state never reports "off" while the motor is
    // still commutating. Read the device's own session slot the same
    // way `MountDevice::send` does.
    let guard = session_slot.read().await;
    let Some(session) = guard.as_ref() else {
        // Disconnected between the gate and here — nothing to stop.
        return false;
    };
    match manager.send(session, Command::StopMotion(Axis::Ra)).await {
        Ok(_) => {
            drop(guard);
            state.write().await.tracking_requested = false;
            warn!(
                mech_ha,
                zone_min = zone.0,
                zone_max = zone.1,
                margin,
                "tracking-guard: encoder mech_HA entered the CW exclusion-zone margin while \
                 tracking; stopped the mount (:K1). Tracking is now off — flip via SetSideOfPier, \
                 slew elsewhere, or park before re-engaging."
            );
            true
        }
        Err(e) => {
            // Leave `tracking_requested` set so `Tracking` keeps
            // reporting the wire truth; the next tick retries while
            // `mech_HA` stays in the band.
            warn!(
                error = %e,
                mech_ha,
                "tracking-guard: failed to stop tracking on zone approach; will retry next tick"
            );
            false
        }
    }
}

/// One auto-flip evaluation against the latest cached snapshot.
///
/// Takes and returns the once-per-crossing attempt latch: `true` means
/// an attempt was already made for the current meridian crossing. The
/// latch re-arms (returns to `false`) whenever the encoder `mech_HA`
/// reads below the configured offset. In the folded `[−12, +12)`
/// encoder space a successful flip lands near `offset − 12 h` for a
/// non-negative offset (re-arms immediately) but near `offset + 12 h`
/// for a negative one — there the latch holds, inert (the pier-side
/// gate below already makes the tick a no-op on the flipped side),
/// until tracking folds past `+12 h` at most `|offset|` hours later or
/// a slew lands back east of the offset. The raw `<` needs no fold
/// handling of its own: on the natural side — the only side where the
/// latch gates a flip — tracking moves `mech_HA` monotonically from
/// the slew's landing point up toward the guard band without crossing
/// a fold, and a folded-delta comparison would misread legitimate
/// far-east pointings (`mech_HA` near `−12 h` with a positive offset)
/// as west of the trigger.
///
/// It is a no-op (latch unchanged) when the client has no tracking
/// engaged, a slew is in flight, parameters aren't cached yet, or the
/// mount is not on the natural (pre-flip) pier side — only the
/// natural → flipped direction is automated: on the post-flip side
/// tracking drifts *away* from the CW exclusion zone, and flipping
/// back is the next slew's (or the operator's) decision.
///
/// The flip itself is [`Telescope::set_side_of_pier`] to the opposite
/// side — the driver calls `SetSideOfPier` on the operator's behalf,
/// so every gate and routing rule of the explicit path applies. A
/// failed attempt logs `warn!` and latches (no retry this crossing);
/// the stop-only guard ([`tracking_guard_tick`]) stays the safety
/// fallback while tracking keeps drifting.
pub(super) async fn auto_flip_tick(
    device: &MountDevice,
    offset_hours: f64,
    already_attempted: bool,
) -> bool {
    // Same cheap gates as the guard: only act while the client has
    // tracking engaged and no slew is in flight.
    if !device.state.read().await.tracking_requested
        || device.slew_in_progress.load(Ordering::SeqCst)
    {
        return already_attempted;
    }
    let Some(params) = device.manager.parameters().await else {
        return already_attempted;
    };
    let snap = device.manager.snapshot().await;
    let mech_ha = RaTicks::new(snap.ra.position_ticks)
        .to_mech_ha(Cpr::new(params.cpr_ra))
        .value();
    if mech_ha < offset_hours {
        // East of the trigger: (re-)arm the latch.
        return false;
    }
    if already_attempted {
        return true;
    }
    let current_side = side_of_pier_calc(
        DecTicks::new(snap.dec.position_ticks),
        Cpr::new(params.cpr_dec),
        device.config.site_latitude_deg,
    );
    if current_side != pre_flip_side_for_latitude(device.config.site_latitude_deg) {
        // Post-flip, or Unknown (no encoder classification to anchor a
        // flip on): nothing to automate.
        return already_attempted;
    }
    let target_side = opposite_pier_side(current_side);
    info!(
        mech_ha,
        offset_hours,
        ?target_side,
        "auto-flip: tracking reached the meridian offset; starting a meridian flip"
    );
    if let Err(e) = device.set_side_of_pier(target_side).await {
        warn!(
            error = %e,
            mech_ha,
            "auto-flip: flip attempt failed; not retrying this crossing — the \
             tracking guard remains the safety fallback"
        );
    }
    // On success the flip slew is in flight and the slew-completion watcher
    // re-engages tracking on the new pier side; either way the attempt latch
    // is spent for this crossing.
    true
}

/// One iteration of the per-connection watcher loop: the stop-only
/// guard evaluates first, then — only when the guard did not fire and
/// auto-flip is armed — the auto-flip trigger. Returns the updated
/// auto-flip attempt latch.
///
/// Guard precedence is deliberate: a tick that finds `mech_HA` already
/// inside the guarded band means the flip window has effectively been
/// missed, and stopping beats starting a flip from inside the danger
/// margin.
pub(super) async fn guard_loop_tick(
    device: &MountDevice,
    zone: (f64, f64),
    margin: f64,
    auto_flip_armed: bool,
    offset_hours: f64,
    flip_attempted: bool,
) -> bool {
    let stopped = tracking_guard_tick(
        &device.state,
        &device.manager,
        &device.session,
        &device.slew_in_progress,
        zone,
        margin,
    )
    .await;
    if stopped || !auto_flip_armed {
        return flip_attempted;
    }
    auto_flip_tick(device, offset_hours, flip_attempted).await
}

/// Spawn the per-connection tracking-time watcher (safety guard +
/// opt-in auto-flip).
///
/// Called on the `set_connected(true)` 0→1 transition once the session
/// slot is populated, with a clone of the device — clones share the
/// session slot, driver state, slew flag, and manager, so the watcher
/// observes and drives the same connection. The task ticks at
/// `polling_interval` (reusing the background poll loop's cadence),
/// reads the cached snapshot, and acts only when [`guard_loop_tick`]
/// finds tracking engaged and `mech_HA` actionable. It self-terminates
/// when `set_connected(false)` clears the session slot — the same
/// disconnect signal the completion watchers observe.
pub(super) fn spawn_tracking_guard(device: MountDevice, polling_interval: Duration) {
    let zone = device.config.cw_exclusion_zone.bounds();
    let margin = device.config.tracking_guard_margin_hours.value();
    let policy = device.config.flip_policy;
    let offset_hours = policy.auto_flip_at_meridian_offset_hours;
    // Auto-flip acts only under the flip_policy master switch. The
    // finite check is defense-in-depth for construction paths that
    // bypass the config-load cross-field validation.
    let auto_flip_armed =
        policy.enabled && policy.auto_flip_during_tracking && offset_hours.is_finite();
    tokio::spawn(async move {
        let mut ticker = interval(polling_interval);
        // Skip the immediate first tick (matches the background poll
        // loop): the handshake just seeded the snapshot.
        ticker.tick().await;
        let mut flip_attempted = false;
        loop {
            ticker.tick().await;
            if device.session.read().await.is_none() {
                debug!("tracking-guard: session closed, exiting");
                return;
            }
            flip_attempted = guard_loop_tick(
                &device,
                zone,
                margin,
                auto_flip_armed,
                offset_hours,
                flip_attempted,
            )
            .await;
        }
    });
}

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
mod tests {
    use super::tracking_guard_breached;
    use proptest::prelude::*;

    // Default GTi zone for the example-based cases.
    const ZONE: (f64, f64) = (0.95, 11.05);

    #[test]
    fn far_below_the_zone_is_not_breached() {
        // A session starting at mech_HA = −3 h is well clear.
        assert!(!tracking_guard_breached(-3.0, ZONE, 0.05));
    }

    #[test]
    fn inside_the_hard_zone_is_breached_even_with_zero_margin() {
        assert!(tracking_guard_breached(6.0, ZONE, 0.0));
    }

    #[test]
    fn margin_stops_tracking_before_zone_entry() {
        // mech_HA = 0.92 is below the 0.95 zone entry but inside the
        // 0.05 h margin band (0.90, 11.10) — the guard fires early.
        assert!(tracking_guard_breached(0.92, ZONE, 0.05));
        // Without the margin (0.0) the same point is still clear.
        assert!(!tracking_guard_breached(0.92, ZONE, 0.0));
    }

    #[test]
    fn band_edges_are_exclusive() {
        // The band is the open interval (zone_min − margin, zone_max + margin).
        assert!(!tracking_guard_breached(0.95 - 0.05, ZONE, 0.05));
        assert!(!tracking_guard_breached(11.05 + 0.05, ZONE, 0.05));
    }

    #[test]
    fn just_inside_each_edge_is_breached() {
        assert!(tracking_guard_breached(0.95 - 0.05 + 1e-6, ZONE, 0.05));
        assert!(tracking_guard_breached(11.05 + 0.05 - 1e-6, ZONE, 0.05));
    }

    #[test]
    fn disabled_zone_never_breaches() {
        // The world's test config disables the zone with min > max.
        assert!(!tracking_guard_breached(6.0, (24.0, 0.0), 0.05));
        // An equal pair is also empty.
        assert!(!tracking_guard_breached(6.0, (6.0, 6.0), 0.05));
    }

    #[test]
    fn negative_margin_degrades_to_zero() {
        // Treated as 0.0: the band collapses to the raw zone, so a
        // point below zone entry is clear but one inside still fires.
        assert!(!tracking_guard_breached(0.92, ZONE, -1.0));
        assert!(tracking_guard_breached(6.0, ZONE, -1.0));
    }

    #[test]
    fn non_finite_margin_degrades_to_zero() {
        assert!(!tracking_guard_breached(0.92, ZONE, f64::NAN));
        assert!(tracking_guard_breached(6.0, ZONE, f64::INFINITY));
    }

    #[test]
    fn non_finite_mech_ha_is_not_breached() {
        assert!(!tracking_guard_breached(f64::NAN, ZONE, 0.05));
    }

    proptest! {
        /// Any point strictly inside the hard zone is breached for every
        /// non-negative margin — widening the band can't exclude it.
        #[test]
        fn inside_hard_zone_always_breached(
            x in 0.951f64..11.049,
            margin in 0.0f64..2.0,
        ) {
            prop_assert!(tracking_guard_breached(x, ZONE, margin));
        }

        /// Increasing the margin never un-breaches a point: the guarded
        /// band only grows.
        #[test]
        fn margin_widening_is_monotonic(
            x in -12.0f64..12.0,
            m1 in 0.0f64..1.0,
            extra in 0.0f64..1.0,
        ) {
            if tracking_guard_breached(x, ZONE, m1) {
                prop_assert!(tracking_guard_breached(x, ZONE, m1 + extra));
            }
        }
    }
}
