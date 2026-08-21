//! Slew-axis wire helpers and slew-geometry math for the Star
//! Adventurer `GTi` driver.
//!
//! The watcher loops in [`super::watchers`] orchestrate slews and
//! pickups; the inherent slew planner on [`super::MountDevice`] composes
//! these helpers into the user-facing `SlewToCoordinatesAsync`. Both
//! call into this module for:
//!
//! - The INDI-style per-axis wire sequence
//!   (`:K` + poll → `:G` → `:I` → `:H` → `:M` → `:J`) issued by
//!   [`issue_slew_axis`] and [`stop_axis_and_wait`].
//! - The flip-aware delta-routing geometry
//!   ([`flip_slew_ra_delta`], [`flip_slew_dec_delta`] and the path-aware
//!   helpers they depend on) that keeps the CW out of the exclusion
//!   zone and the Dec sweep out of the below-horizon pole.

use std::time::Duration;

use ascom_alpaca::{ASCOMError, ASCOMErrorCode, ASCOMResult};
use rusty_photon_shared_transport::Session;
use skywatcher_motor_protocol::command::{ModeKind, MotionMode, Speed};
use skywatcher_motor_protocol::{Axis, Command, Response};

use crate::codec::SkywatcherCodec;
use crate::coordinates::sidereal_step_period;
use crate::error::StarAdvError;
use crate::manager::{MountManager, MountParameters};
use crate::units::{Cpr, RaTicks};

/// Upper bound on how long [`stop_axis_and_wait`] will poll `:f<axis>`
/// after a `:K` (decelerate stop) before giving up. The firmware
/// finishes deceleration within ~1 s for typical Goto-Fast slew
/// rates on the `GTi`; 2 s is a comfortable margin for the slow
/// case, and bounding the wait prevents a stuck axis from wedging
/// a slew indefinitely.
pub(super) const AXIS_STOP_TIMEOUT: Duration = Duration::from_secs(2);

/// EQMOD `minperiods[axis]` default — see
/// `indi-3rdparty/indi-eqmod/skywatcher.cpp:509-510`. INDI emits
/// `:I<axis>6` on every slew; the firmware uses this step period
/// to ramp the motor through the goto.
const SLEW_STEP_PERIOD: u32 = 6;

/// INDI `SetTargetBreaks` cap — see
/// `indi-3rdparty/indi-eqmod/skywatcher.cpp::SlewTo`. The breakpoint
/// increment is `min(|delta|/10, 3200)`; without the cap, very long
/// slews exceed the firmware's break-point range.
const SLEW_BREAK_POINT_DIVISOR: u32 = 10;
const SLEW_BREAK_POINT_MAX: u32 = 3200;

/// Issue the per-axis INDI slew sequence:
/// `:G<axis>` (goto + fast, direction by sign of `delta`) →
/// `:I<axis>6` (step period) →
/// `:H<axis><|delta|>` (target increment) →
/// `:M<axis><breaks>` (break-point increment) →
/// `:J<axis>` (start motion).
///
/// The caller must have already issued `:K<axis>` and waited for the
/// running flag to clear — `:G` returns `!2 MotorNotStopped` if the
/// motor is still decelerating from a prior command.
pub(super) async fn issue_slew_axis(
    manager: &MountManager,
    session: &Session<SkywatcherCodec>,
    axis: Axis,
    delta: i32,
) -> crate::error::Result<()> {
    let magnitude = delta.unsigned_abs();
    let breaks = (magnitude / SLEW_BREAK_POINT_DIVISOR).min(SLEW_BREAK_POINT_MAX);
    let mode = MotionMode {
        kind: ModeKind::Goto,
        speed: Speed::Fast,
        ccw: delta < 0,
    };
    manager
        .send(session, Command::SetMotionMode { axis, mode })
        .await?;
    manager
        .send(
            session,
            Command::SetStepPeriod {
                axis,
                period: SLEW_STEP_PERIOD,
            },
        )
        .await?;
    manager
        .send(
            session,
            Command::SetGotoTargetIncrement {
                axis,
                increment: magnitude,
            },
        )
        .await?;
    manager
        .send(session, Command::SetBreakPointIncrement { axis, breaks })
        .await?;
    manager.send(session, Command::StartMotion(axis)).await?;
    Ok(())
}

/// Issue `:K<axis>` (decelerate) and poll `:f<axis>` until the
/// running flag clears or `timeout` elapses. `:K` is the spec's
/// recommended stop and is gentler on the gearbox than `:L`; `:L`
/// remains the right choice only for genuine emergency stops
/// (`AbortSlew`, slew/park watcher abort on `blocked`). Matches INDI
/// eqmod's `StopWaitMotor` (`indi-eqmod/skywatcher.cpp:1741-1765`).
///
/// Production callers pass [`AXIS_STOP_TIMEOUT`]; the parameter is
/// only an indirection for tests that want a much shorter bound to
/// stay fast on a stuck-axis simulation.
pub(super) async fn stop_axis_and_wait(
    manager: &MountManager,
    session: &Session<SkywatcherCodec>,
    axis: Axis,
    timeout: Duration,
) -> crate::error::Result<()> {
    manager.send(session, Command::StopMotion(axis)).await?;
    let start = std::time::Instant::now();
    tokio::time::sleep(Duration::from_millis(100)).await;
    loop {
        let resp = manager.send(session, Command::InquireStatus(axis)).await?;
        if let Response::Status(s) = resp {
            if !s.motion.running {
                return Ok(());
            }
        }
        if start.elapsed() >= timeout {
            return Err(StarAdvError::Transport(format!(
                "axis {axis:?} did not stop within {timeout:?}"
            )));
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

/// Re-engage sidereal tracking on the RA axis. Issues the canonical
/// three-step sequence: `:G1 TRACKING` → `:I1 sidereal_period` → `:J1`.
///
/// Caller is responsible for the prior `:K1` + stop-wait — `:G` returns
/// `!2 MotorNotStopped` if the motor is still decelerating. Returns on
/// first wire failure so the caller picks the policy:
/// `set_tracking(true)` maps to `ASCOMError` and propagates; the slew /
/// pulse-guide watchers log at `warn` and continue.
pub(super) async fn enable_sidereal_tracking_ra(
    manager: &MountManager,
    session: &Session<SkywatcherCodec>,
    params: &MountParameters,
) -> crate::error::Result<()> {
    let period = sidereal_step_period(params.tmr_freq, Cpr::new(params.cpr_ra));
    manager
        .send(
            session,
            Command::SetMotionMode {
                axis: Axis::Ra,
                mode: MotionMode::TRACKING,
            },
        )
        .await?;
    manager
        .send(
            session,
            Command::SetStepPeriod {
                axis: Axis::Ra,
                period,
            },
        )
        .await?;
    manager
        .send(session, Command::StartMotion(Axis::Ra))
        .await?;
    Ok(())
}

/// Per-axis pickup re-slew used by the watcher's EQMOD pickup loop.
/// Calls [`stop_axis_and_wait`] (drains any residual goto deceleration)
/// then [`issue_slew_axis`] (re-runs the INDI wire sequence with the
/// freshly-computed `delta`). Both calls are best-effort: a failure
/// from either is logged at `warn` and swallowed because the watcher
/// has nothing useful to do with the error other than retry on the
/// next iteration. Wrapping the pair in this helper keeps the watcher
/// body free of nested `if let Err` branches that codecov flags as
/// uncovered for the rare-but-real failure paths.
pub(super) async fn pickup_reslew_axis(
    manager: &MountManager,
    session: &Session<SkywatcherCodec>,
    axis: Axis,
    delta: i32,
) {
    if let Err(e) = stop_axis_and_wait(manager, session, axis, AXIS_STOP_TIMEOUT).await {
        tracing::warn!("pickup stop {axis:?} failed: {e}");
        return;
    }
    if let Err(e) = issue_slew_axis(manager, session, axis, delta).await {
        tracing::warn!("pickup re-slew {axis:?} failed: {e}");
    }
}

/// Force a flip slew's RA delta to keep the polar-axis sweep out of
/// the CW exclusion zone `mech_HA ∈ (zone_min, zone_max)`
/// (default `(+0.95, +11.05)` on the `GTi` — the arc where the CW
/// rises more than 0.95 h above horizontal).
///
/// The CW exclusion zone is at positive `mech_HA` only and is a
/// structural property of the mount head independent of observer
/// latitude. Both forward flips (pre-flip → post-flip) and flip-backs
/// (post-flip → pre-flip) need their RA paths constrained.
///
/// Strategy: take the canonical short path unless its linear `mech_HA`
/// sweep from `current_ticks` through `current_ticks + canonical_delta`
/// crosses the CW exclusion zone (modulo the 24-hour wrap). If it would,
/// try the long way around (`canonical ± cpr_i`) which lands at the
/// same modular destination via the safe arc on the other side. If
/// the long way *also* crosses the zone, there is no safe RA path
/// between current and target and the slew is refused with
/// `INVALID_OPERATION`.
///
/// Previously a sign-blind heuristic (`|current| > cpr/4 ⇒ "safe is
/// positive"`) was used. That mis-fired at Park 4 N
/// (current ≈ -cpr/2, canonical ≈ -4k CCW just past the wrap): the
/// heuristic flipped the small CCW step into a +cpr/2 + small CW full
/// revolution that swept across the zone and slammed the CW shaft
/// into the pier (hardware validation 2026-05-16). The path-aware
/// check uses the actual CW exclusion zone, so it preserves the safe
/// canonical step when it doesn't cross. The both-cross refusal was
/// added after the 2026-05-17 session, where a `SetSideOfPier`
/// from Park 3 produced a `canonical_delta = -cpr/2` whose long-way
/// alternative `+cpr/2` swept the OTA through the tripod region with
/// the narrow `(+6.95, +11.05)` zone permitting it. With the wider
/// `(+0.95, +11.05)` zone both directions cross and the slew is now
/// rejected.
pub(super) fn flip_slew_ra_delta(
    canonical_delta: i32,
    current_ticks: i32,
    cpr: u32,
    binding_zone_hours: (f64, f64),
) -> ASCOMResult<i32> {
    if cpr == 0 || canonical_delta == 0 {
        return Ok(canonical_delta);
    }
    let cpr_i = cpr.cast_signed();
    let cur_ha = RaTicks::new(current_ticks)
        .to_mech_ha(Cpr::new(cpr))
        .value();
    let delta_ha = f64::from(canonical_delta) * 24.0 / f64::from(cpr);
    if !canonical_path_crosses_binding_zone(cur_ha, delta_ha, binding_zone_hours) {
        return Ok(canonical_delta);
    }
    // `canonical_delta` is folded to `[-cpr/2, +cpr/2)` and `cpr` is a
    // 24-bit wire field, so the long-way correction stays within `(-cpr,
    // +cpr)` — saturating is its total spelling.
    let long_way = if canonical_delta > 0 {
        canonical_delta.saturating_sub(cpr_i)
    } else {
        canonical_delta.saturating_add(cpr_i)
    };
    let long_delta_ha = f64::from(long_way) * 24.0 / f64::from(cpr);
    if !canonical_path_crosses_binding_zone(cur_ha, long_delta_ha, binding_zone_hours) {
        return Ok(long_way);
    }
    Err(ASCOMError::new(
        ASCOMErrorCode::INVALID_OPERATION,
        format!(
            "no safe RA path from mech_HA {cur_ha:+.3} h: canonical short ({delta_ha:+.3} h) \
             and long-way around ({long_delta_ha:+.3} h) both cross the CW exclusion zone \
             ({zone_min:+.3}, {zone_max:+.3})",
            zone_min = binding_zone_hours.0,
            zone_max = binding_zone_hours.1,
        ),
    ))
}

/// Verify a non-flip RA slew's canonical sweep doesn't cross the
/// CW exclusion zone. Flip slews have the option of taking the long way
/// around via [`flip_slew_ra_delta`]; non-flip slews don't — the
/// canonical short delta is the unique path between current and
/// target on the chosen pier side, so if it crosses the zone the
/// slew is refused.
pub(super) fn check_non_flip_ra_path(
    canonical_delta: i32,
    current_ticks: i32,
    cpr: u32,
    binding_zone_hours: (f64, f64),
) -> ASCOMResult<()> {
    if cpr == 0 || canonical_delta == 0 {
        return Ok(());
    }
    let cur_ha = RaTicks::new(current_ticks)
        .to_mech_ha(Cpr::new(cpr))
        .value();
    let delta_ha = f64::from(canonical_delta) * 24.0 / f64::from(cpr);
    if !canonical_path_crosses_binding_zone(cur_ha, delta_ha, binding_zone_hours) {
        return Ok(());
    }
    Err(ASCOMError::new(
        ASCOMErrorCode::INVALID_OPERATION,
        format!(
            "non-flip RA slew from mech_HA {cur_ha:+.3} h by {delta_ha:+.3} h crosses the \
             CW exclusion zone ({zone_min:+.3}, {zone_max:+.3})",
            zone_min = binding_zone_hours.0,
            zone_max = binding_zone_hours.1,
        ),
    ))
}

/// Does the linear `mech_HA` sweep from `start_ha` by `delta_ha` enter
/// `(zone_min, zone_max)` (modulo 24 h)? The sweep is the open
/// interval `(min(start, start+delta), max(start, start+delta))`; the
/// CW exclusion zone repeats every 24 hours, so we check `k ∈ {-1, 0, +1}`
/// — enough to cover any `|delta_ha| ≤ 12` path. An empty zone
/// (`zone_min ≥ zone_max`) is treated as no zone.
#[expect(
    clippy::suboptimal_flops,
    reason = "zone + 24·k is the day-period unwrap over exact small values; nothing to fuse"
)]
fn canonical_path_crosses_binding_zone(
    start_ha: f64,
    delta_ha: f64,
    binding_zone_hours: (f64, f64),
) -> bool {
    let (zone_min, zone_max) = binding_zone_hours;
    if zone_min >= zone_max {
        return false;
    }
    let path_lo = start_ha.min(start_ha + delta_ha);
    let path_hi = start_ha.max(start_ha + delta_ha);
    for k in [-1.0_f64, 0.0, 1.0] {
        let bz_lo = zone_min + 24.0 * k;
        let bz_hi = zone_max + 24.0 * k;
        // Open-interval overlap: paths grazing the boundary stay safe.
        if path_lo < bz_hi && bz_lo < path_hi {
            return true;
        }
    }
    false
}

/// Force a flip slew's Dec delta to traverse the **visible** celestial
/// pole rather than the below-horizon pole.
///
/// During a Dec flip-slew the encoder must cross one of the `±cpr/4`
/// boundaries (the two celestial poles). For a polar-aligned mount,
/// only ONE pole is above the local horizon: NCP at altitude `+lat`
/// for northern observers (encoder `+cpr/4`), SCP at altitude `+|lat|`
/// for southern (encoder `−cpr/4`). The other pole is below the
/// horizon and the path through it dips the OTA below the local
/// horizon — exactly the failure mode hit during the first hardware
/// validation when the OTA was driven through SCP at lat 32.7°N.
///
/// Strategy (mirroring [`flip_slew_ra_delta`]): take the canonical
/// short path unless its linear dec-encoder sweep from `current_ticks`
/// through `current_ticks + canonical_delta` crosses the
/// below-horizon-pole encoder position at any modular replica
/// (`−cpr/4` for N, `+cpr/4` for S). If it would, take the long way
/// around (`canonical ± cpr_dec`), which lands at the same modular
/// destination via the safe pole.
///
/// Previously a sign-blind heuristic (`|current| ≤ cpr/4 ⇒ "safe is
/// positive (toward NCP for N)"`) was used. It happens to give the
/// right answer for every realistic flip-slew (current, target) pair
/// after folding raw to the canonical band, but the path-aware check
/// makes the safety property explicit, mirrors the RA helper's
/// structure, and is naturally robust to `current_ticks` outside the
/// canonical band (the modular-replica scan covers all continuous
/// sweeps regardless of where raw has accumulated).
pub(super) const fn flip_slew_dec_delta(
    canonical_delta: i32,
    current_ticks: i32,
    cpr_dec: u32,
    northern: bool,
) -> i32 {
    if cpr_dec == 0 || canonical_delta == 0 {
        return canonical_delta;
    }
    let cpr_i = cpr_dec.cast_signed();
    let unsafe_pole = if northern {
        (cpr_i / 4).saturating_neg()
    } else {
        cpr_i / 4
    };
    if !canonical_path_crosses_pole(current_ticks, canonical_delta, unsafe_pole, cpr_dec) {
        return canonical_delta;
    }
    // Same bound as [`flip_slew_ra_delta`]'s long way: a folded delta
    // corrected by one wire-width CPR stays within `(-cpr, +cpr)`.
    if canonical_delta > 0 {
        canonical_delta.saturating_sub(cpr_i)
    } else {
        canonical_delta.saturating_add(cpr_i)
    }
}

/// True iff the linear encoder sweep `[start, start + delta]` crosses
/// any modular replica of `pole_ticks`. Used by [`flip_slew_dec_delta`]
/// to detect when the canonical short path would dip the OTA through
/// the below-horizon pole.
///
/// `start` can sit anywhere in the signed-24-bit wire range
/// (`±2²³ ≈ ±8.4M ticks`), which for the `GTi`'s `cpr_dec ≈ 3.6M` puts
/// the relevant modular replica index up to `k = ±3` away from zero.
/// Rather than enumerating a fixed `k` window, the check shifts the
/// sweep into pole-relative coordinates `[a, b] = [lo − pole, hi − pole]`
/// and tests whether that interval contains any integer multiple of
/// `cpr` — equivalent to the largest multiple `≤ b` being `≥ a`.
pub(super) const fn canonical_path_crosses_pole(
    start: i32,
    delta: i32,
    pole_ticks: i32,
    cpr: u32,
) -> bool {
    let cpr_i = cpr.cast_signed();
    // Every operand originates in a 24-bit wire field (positions ±2²³,
    // CPR ≤ 2²⁴), so these sums sit far inside `i32`; saturating is the
    // total spelling, and an impossible operand degrades to a clamped
    // sweep interval instead of a release-mode wrap onto the wrong side
    // of the check.
    let end = start.saturating_add(delta);
    let (lo, hi) = if end >= start {
        (start, end)
    } else {
        (end, start)
    };
    let a = lo.saturating_sub(pole_ticks);
    let b = hi.saturating_sub(pole_ticks);
    // Largest multiple of `cpr_i` that is `≤ b`. If that multiple is
    // also `≥ a` it lies inside `[a, b]`, so the corresponding pole
    // replica `pole_ticks + k·cpr` lies inside `[lo, hi]`.
    b.div_euclid(cpr_i).saturating_mul(cpr_i) >= a
}
