//! Slew, park and pulse-guide completion watchers spawned by
//! [`super::MountDevice`].
//!
//! Each watcher is a tokio task that observes mount state in the
//! background, applies the per-operation completion semantics (EQMOD
//! pickup loop, post-slew tracking restore, `at_park = true`, axis
//! restore after pulse), and clears the `slew_in_progress` /
//! `pulse_guiding_<axis>` flag so the user-visible ASCOM state lines
//! up with the wire state.
//!
//! The slew and park completion watchers share an identical outer
//! loop — pause polling, sleep one tick, honour abort / disconnect /
//! `:f` Blocked, skip while either axis is `running`, then run the
//! per-operation completion step once both axes report stopped. That
//! shared scaffold lives in [`run_completion_watcher`]; each spawner
//! supplies an `on_axes_stopped` closure returning a
//! [`CompletionDecision`] plus a [`FnOnce(&mut DriverState)`]
//! finalizer that lands the per-operation state mutation under a
//! `DriverState` write lock, after which the watcher clears the
//! `slew_in_progress` atomic. The pulse-guide
//! watcher has a different shape (no polling loop, axis-targeted
//! restore) and stays as a standalone spawner.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, SystemTime};

use ascom_alpaca::api::telescope::PierSide;
use rusty_photon_shared_transport::Session;
use skywatcher_motor_protocol::{Axis, Command};
use tokio::sync::RwLock;
use tracing::debug;

use crate::codec::SkywatcherCodec;
use crate::config::MountConfig;
use crate::coordinates::{
    encoder_to_celestial, local_sidereal_time_hours, pickup_target_ra_ticks, target_encoder_flipped,
};
use crate::error::StarAdvError;
use crate::manager::{MountManager, MountSnapshot};
use crate::units::{Cpr, Dec, DecTicks, Lst, Ra, RaTicks};

use super::slew::{
    enable_sidereal_tracking_ra, pickup_reslew_axis, stop_axis_and_wait, AXIS_STOP_TIMEOUT,
};
use super::{pre_flip_side_for_latitude, DriverState};

/// Shared session slot the device holds. Watchers peek for `is_none()`
/// to detect "user disconnected" — independent of their own session
/// keeping the shared transport alive while in-flight work finishes.
type SessionSlot = Arc<RwLock<Option<Session<SkywatcherCodec>>>>;

/// Test whether the user's `set_connected(false)` has cleared the
/// device's session. Held briefly (no wire I/O while the read lock is
/// owned), so a concurrent `set_connected(false)` write only waits
/// for this check to complete.
async fn user_disconnected(slot: &SessionSlot) -> bool {
    slot.read().await.is_none()
}

/// Minimum wallclock duration the slew watcher will keep
/// `slew_in_progress` set, regardless of how fast the mount reports
/// the goto complete. See the rationale in
/// [`spawn_slew_completion_watcher`]: it guarantees that an Alpaca
/// client polling `Slewing` shortly after issuing a slew will catch
/// the `true` value at least once. Empirically `ConformU`'s
/// AbortSlew-test wait between starting the slew and reading
/// `Slewing` runs in the 1.0–1.5 s range, so the floor needs to be
/// noticeably above that — 2 s is comfortable. A real `GTi` slew of
/// any meaningful distance takes well over 2 s, so this floor is
/// invisible on hardware.
const MIN_SLEW_DWELL: Duration = Duration::from_secs(2);

/// EQMOD `RAGOTORESOLUTION` / `DEGOTORESOLUTION` — see
/// `indi-3rdparty/indi-eqmod/eqmodbase.cpp:64-66`. After the goto
/// stops, the pickup loop computes the residual against the latched
/// RA/Dec target and re-issues a corrective slew if either axis
/// exceeds this threshold (5 arc-seconds).
const PICKUP_TOLERANCE_ARCSEC: f64 = 5.0;

/// EQMOD `GOTO_ITERATIVE_LIMIT` — see
/// `indi-3rdparty/indi-eqmod/eqmodbase.cpp:64`. INDI caps the
/// pickup loop at 5 iterations to keep a pathological case (motor
/// stalled, encoder oscillating, …) from running forever.
const PICKUP_MAX_ITERATIONS: u32 = 5;

/// Consecutive `poll_axes_now` failures the slew/park watcher
/// tolerates before giving up. A single transient USB-CDC glitch
/// (queue flush race, brief renumeration, …) recovers within one
/// frame and shouldn't take the watcher offline for the rest of
/// the slew — the original "one strike and exit" policy meant any
/// pre-binding hiccup left a runaway motor with no observer. Three
/// attempts × [`WATCHER_POLL_RETRY_BACKOFF`] keeps the cumulative
/// recovery window well inside the polling cadence so a genuinely
/// blocked axis is still detected within ~1 s of the firmware
/// latching the bit.
const WATCHER_POLL_RETRY_LIMIT: u32 = 3;

/// Sleep between consecutive `poll_axes_now` retry attempts in the
/// slew/park watcher. Short enough that the cumulative
/// `WATCHER_POLL_RETRY_LIMIT × WATCHER_POLL_RETRY_BACKOFF` budget
/// stays inside the next polling tick; long enough that a tokio-
/// serial read can flush whatever junk the kernel buffered during
/// a brief CDC glitch before the next attempt.
const WATCHER_POLL_RETRY_BACKOFF: Duration = Duration::from_millis(100);

/// Returns `true` when the slew-completion watcher must bail out of
/// its current iteration: either `AbortSlew` cleared
/// `slew_in_progress`, or `set_connected(false)` closed the transport.
/// Both conditions can race in mid-iteration after the top-of-loop
/// guard has already passed, so the watcher checks this helper a
/// second time immediately before issuing any post-snapshot wire
/// commands (the EQMOD pickup re-slew or the post-slew tracking
/// restart).
pub(super) async fn watcher_should_abort(
    slew_in_progress: &AtomicBool,
    session_slot: &SessionSlot,
) -> bool {
    !slew_in_progress.load(Ordering::SeqCst) || user_disconnected(session_slot).await
}

/// Retrying wrapper around [`MountManager::poll_axes_now`] used by
/// both the slew and park completion watchers. Tolerates up to
/// [`WATCHER_POLL_RETRY_LIMIT`] consecutive transport errors so a
/// single transient USB-CDC glitch (a brief renumeration, a stale
/// kernel buffer, …) doesn't take the watcher offline for the rest
/// of a goto.
///
/// On every successful poll the snapshot is emitted at `debug` so a
/// post-mortem can reconstruct the last-known-good state observed
/// before any failure. On every failed attempt the underlying error
/// is logged at `warn` with the attempt counter.
///
/// On retry exhaustion, the helper makes a best-effort `:L` on both
/// axes before returning the underlying error: even when we can no
/// longer observe state, the firmware may still be commutating step
/// pulses, and a runaway motor with no observer is the worst case
/// the original exit-on-first-error policy created. The `:L` calls
/// are fire-and-forget — if they fail too, there's nothing useful
/// the watcher can do beyond logging and bailing.
pub(super) async fn watcher_poll_with_retry(
    manager: &MountManager,
    session: &Session<SkywatcherCodec>,
    context: &'static str,
) -> crate::error::Result<MountSnapshot> {
    let mut last_err: Option<StarAdvError> = None;
    for attempt in 1..=WATCHER_POLL_RETRY_LIMIT {
        match manager.poll_axes_now(session).await {
            Ok(snap) => {
                debug!(
                    context = context,
                    ra_ticks = snap.ra.position_ticks,
                    ra_running = snap.ra.running,
                    ra_blocked = snap.ra.blocked,
                    ra_goto = snap.ra.goto,
                    dec_ticks = snap.dec.position_ticks,
                    dec_running = snap.dec.running,
                    dec_blocked = snap.dec.blocked,
                    dec_goto = snap.dec.goto,
                    "watcher snapshot"
                );
                return Ok(snap);
            }
            Err(e) => {
                tracing::warn!(
                    context = context,
                    attempt = attempt,
                    limit = WATCHER_POLL_RETRY_LIMIT,
                    "watcher poll_axes_now transient error: {e}"
                );
                last_err = Some(e);
                if attempt < WATCHER_POLL_RETRY_LIMIT {
                    tokio::time::sleep(WATCHER_POLL_RETRY_BACKOFF).await;
                }
            }
        }
    }
    tracing::warn!(
        context = context,
        "watcher poll_axes_now retries exhausted — best-effort :L on both axes before bailing"
    );
    let _ = manager.send(session, Command::InstantStop(Axis::Ra)).await;
    let _ = manager.send(session, Command::InstantStop(Axis::Dec)).await;
    Err(last_err
        .unwrap_or_else(|| StarAdvError::Transport("watcher poll retries exhausted".to_string())))
}

/// Clear the per-axis `pulse_guiding_<axis>` flag. `GuideDirection`
/// only resolves to `Ra` or `Dec` (see the direction-to-axis match in
/// `MountDevice::pulse_guide`), so this helper never sees
/// `Axis::Both`. Using a boolean dispatch keeps the code exhaustive
/// without an unreachable arm.
pub(super) async fn clear_pulse_flag(state: &Arc<RwLock<DriverState>>, axis: Axis) {
    let mut s = state.write().await;
    if axis == Axis::Ra {
        s.pulse_guiding_ra = false;
    } else {
        s.pulse_guiding_dec = false;
    }
}

/// Decision returned by an `on_axes_stopped` closure after both
/// axes report stopped. Drives the branching at the bottom of
/// [`run_completion_watcher`]'s loop.
enum CompletionDecision {
    /// Stay in the polling loop — e.g. the slew dwell gate has not
    /// elapsed yet, or a pickup re-slew was issued and the next
    /// snapshot will tell us whether it converged.
    Continue,
    /// Bail out without running the settle delay or the finalizer.
    /// Used when an in-flight abort, disconnect, or computation
    /// error is detected after the snapshot was taken. The helper
    /// clears `slew_in_progress` on the way out.
    Bail,
    /// Per-operation work is done. The helper drops the polling
    /// guard, sleeps `settle`, runs the finalizer under a
    /// `DriverState` write lock, then clears the `slew_in_progress`
    /// atomic.
    Complete,
}

/// Shared outer-loop scaffold for the slew and park completion
/// watchers. Both watchers observe the same six top-of-loop steps:
///
/// 1. Sleep one `polling_interval` tick.
/// 2. Bail if `slew_in_progress` was cleared externally (`AbortSlew`,
///    `set_connected(false)`).
/// 3. Bail if the transport became unavailable.
/// 4. Snapshot via [`watcher_poll_with_retry`].
/// 5. Honour `blocked` reads with `:L` on both axes + bail.
/// 6. Skip iterations where either axis is still `running`.
///
/// Once both axes report stopped, the per-operation `on_axes_stopped`
/// closure runs and returns a [`CompletionDecision`]:
///
/// - [`CompletionDecision::Continue`] — re-enter the loop (the slew
///   watcher uses this for the dwell gate and for pickup re-slews).
/// - [`CompletionDecision::Bail`] — clear `slew_in_progress` and exit
///   without running the settle delay or the finalizer.
/// - [`CompletionDecision::Complete`] — drop the polling guard
///   *before* the settle delay so the background polling task can
///   refresh the snapshot while we wait (an Alpaca client reading
///   position data right after `Slewing` flips to `false` then sees
///   a snapshot that reflects the current encoder state, not the
///   watcher's last `poll_axes_now` from before the completion step;
///   without this the reported RA lags by `(tracking_engagement +
///   settle) × sidereal rate`). After the settle, the finalizer
///   runs under a `DriverState` write lock, then the watcher clears
///   the `slew_in_progress` atomic.
///
/// The polling-pause guard is owned by an in-scope binding so every
/// exit path releases it: the `Complete` branch drops it explicitly
/// before the settle delay (so the background polling task can
/// refresh the snapshot while we wait), and every other path —
/// `Continue` looping, `Bail`, abort, disconnect, blocked-axis, retry
/// exhaustion, panic — releases it via RAII drop on the way out.
#[allow(clippy::too_many_arguments)]
async fn run_completion_watcher<C, F>(
    state: Arc<RwLock<DriverState>>,
    manager: Arc<MountManager>,
    session: Session<SkywatcherCodec>,
    session_slot: SessionSlot,
    slew_in_progress: Arc<AtomicBool>,
    polling_interval: Duration,
    settle: Duration,
    context: &'static str,
    mut on_axes_stopped: C,
    on_finalize: F,
) where
    C: AsyncFnMut(MountSnapshot, &Session<SkywatcherCodec>) -> CompletionDecision,
    F: FnOnce(&mut DriverState),
{
    // Pause the background polling task for the duration of the
    // operation. With polling paused the watcher owns the wire:
    // wire commands (pickup re-slews, post-slew tracking restart,
    // blocked-axis `:L`) fire without contending with `:j` / `:f`
    // polls for `command_lock`, and the watcher's own
    // `poll_axes_now` reads give us mount state within one wire
    // round-trip of any change — vs up to `polling_interval` of
    // snapshot staleness under the always-on polling model.
    let poll_guard = manager.pause_background_polling();
    loop {
        tokio::time::sleep(polling_interval).await;

        // External abort / disconnect path: AbortSlew clears
        // `slew_in_progress` before issuing :L; set_connected(false)
        // also clears it. Either way, bail before overwriting
        // user-visible state.
        if !slew_in_progress.load(Ordering::SeqCst) {
            break;
        }
        // Belt-and-braces: if the user disconnected, exit even if
        // the flag-clear hasn't happened yet. The watcher's own
        // session still holds a refcount on the shared transport,
        // so `manager.is_available()` would lie — we look at the
        // device's session slot instead.
        if user_disconnected(&session_slot).await {
            slew_in_progress.store(false, Ordering::SeqCst);
            break;
        }

        // Direct poll via the watcher's own session, instead of
        // reading the (now-paused) background snapshot.
        // [`watcher_poll_with_retry`] tolerates a handful of
        // transient transport errors so a single USB-CDC glitch
        // doesn't take the watcher offline mid-operation; on retry
        // exhaustion it also issues a best-effort `:L` on both
        // axes so the motor isn't left commutating with no
        // observer.
        let Ok(snap) = watcher_poll_with_retry(&manager, &session, context).await else {
            slew_in_progress.store(false, Ordering::SeqCst);
            break;
        };
        // Sky-Watcher spec §5 reports `Blocked` in the `:f` status
        // when the motor is stepping but the encoder isn't
        // advancing — typically the axis is against a hard stop.
        // Issue `:L` on both axes to halt the runaway and bail out
        // of the operation rather than letting the watcher
        // poll-loop continue while the gearbox strains. Park-time
        // blocked aborts deliberately skip `at_park = true` (the
        // OTA isn't at the encoder-0 home pose, so subsequent
        // `Unpark + slew` would compute a wrong delta) — which is
        // enforced here by returning before the finalizer runs.
        if snap.ra.blocked || snap.dec.blocked {
            tracing::warn!(
                ra_blocked = snap.ra.blocked,
                dec_blocked = snap.dec.blocked,
                context = context,
                "axis reports Blocked — aborting via :L"
            );
            let _ = manager.send(&session, Command::InstantStop(Axis::Ra)).await;
            let _ = manager
                .send(&session, Command::InstantStop(Axis::Dec))
                .await;
            slew_in_progress.store(false, Ordering::SeqCst);
            break;
        }
        if snap.ra.running || snap.dec.running {
            continue;
        }
        match on_axes_stopped(snap, &session).await {
            CompletionDecision::Continue => {}
            CompletionDecision::Bail => {
                slew_in_progress.store(false, Ordering::SeqCst);
                break;
            }
            CompletionDecision::Complete => {
                // Resume background polling *now*, before the
                // settle delay. From here on the watcher is just
                // waiting for any firmware engagement (e.g. the
                // ~160 ms post-slew tracking startup) and applying
                // the settle margin. While we wait, the background
                // polling task should refresh the snapshot at its
                // regular cadence so an Alpaca client reading
                // position state right after `Slewing` flips to
                // `false` sees a snapshot that reflects the
                // post-completion encoder state, not the watcher's
                // last `poll_axes_now` from before the completion
                // step. Without this, the snap is stale by the
                // duration `(per-op completion work + settle)` and
                // the reported RA lags by that × sidereal rate
                // (~5-10″).
                drop(poll_guard);
                tokio::time::sleep(settle).await;
                {
                    let mut s = state.write().await;
                    on_finalize(&mut s);
                }
                slew_in_progress.store(false, Ordering::SeqCst);
                break;
            }
        }
    }
    // Close the watcher's session on every exit path. If the user has
    // disconnected and this is the last session, the shared transport
    // tears down (halt sequence + close) inside `close().await`.
    if let Err(e) = session.close().await {
        tracing::warn!(context, error = %e, "watcher session close failed");
    }
}

/// Slew-watcher per-iteration completion step. Called by
/// [`run_completion_watcher`] each time both axes report stopped.
/// Returns:
///
/// - [`CompletionDecision::Continue`] while the [`MIN_SLEW_DWELL`]
///   floor has not elapsed, or after a pickup re-slew has been
///   issued.
/// - [`CompletionDecision::Bail`] when an in-flight abort/disconnect
///   or an LST computation failure is detected.
/// - [`CompletionDecision::Complete`] once the pickup loop has
///   converged (or the iteration limit reached, or no target coords
///   are available) and any post-slew tracking restart has been
///   issued.
///
/// `state`, `transport` and `config` are passed by value (the Arcs
/// cloned, the config cloned cheaply) so the returned future owns
/// them outright rather than borrowing from the caller's closure
/// captures. This keeps the future `Send + 'static`-compatible
/// inside the tokio task spawned by
/// [`spawn_slew_completion_watcher`]; an `&Arc<…>` borrow tripped
/// the HRTB inference on the spawn future's `Send` bound.
#[allow(clippy::too_many_arguments)]
async fn slew_completion_step(
    state: Arc<RwLock<DriverState>>,
    manager: Arc<MountManager>,
    session_slot: SessionSlot,
    slew_in_progress: Arc<AtomicBool>,
    session: &Session<SkywatcherCodec>,
    config: MountConfig,
    polling_interval: Duration,
    started: std::time::Instant,
    tracking_was_on: bool,
    pickup_iterations: &mut u32,
    last_pickup_at: &mut Option<std::time::Instant>,
    snap: MountSnapshot,
) -> CompletionDecision {
    // Enforce a minimum slew dwell so external observers reliably
    // catch `Slewing == true`. ConformU starts a slew via HTTP,
    // then reads `Slewing` over a second HTTP call; the round-
    // trip latency can be larger than the mock's full slew
    // duration on a fast machine (the mock advances 100K
    // ticks/poll, so a small slew completes in 1-2 polls). The
    // de-facto Alpaca client poll cadence is on the order of
    // 100 ms; two full seconds of guaranteed dwell is a safe
    // floor for any reasonable client without meaningfully
    // slowing real-mount operation (real slews take seconds).
    //
    // The dwell *must* gate the pickup loop, not run after it.
    // The encoder is static while the watcher is observing
    // (tracking is off until the post-slew re-enable below),
    // so the apparent RA drifts at sidereal rate as LST
    // advances. If the pickup loop ran during the dwell wait,
    // it would re-detect that drift on every iteration and
    // burn through `PICKUP_MAX_ITERATIONS` just waiting —
    // potentially leaving a residual of one dwell-worth of
    // sidereal drift (~30") at the moment tracking re-enables.
    // Gating pickup behind the dwell means the loop sees a
    // single accumulated residual once, corrects it, then
    // hands off to tracking immediately.
    if started.elapsed() < MIN_SLEW_DWELL {
        return CompletionDecision::Continue;
    }

    // Both axes report stopped and the dwell has elapsed. Run
    // the EQMOD pickup loop: if either residual exceeds 5",
    // re-enter the goto sequence with a fresh delta computed
    // for the current LST. Capped at `PICKUP_MAX_ITERATIONS`
    // to match INDI's `GOTO_ITERATIVE_LIMIT`. On the GTi the
    // loop converges in 1–2 iterations because the post-stop
    // residual is bounded by the slew duration × sidereal
    // rate (~15"/s of RA drift per second of slew).
    if *pickup_iterations < PICKUP_MAX_ITERATIONS {
        let (target_ra, target_dec, target_pier_side) = {
            let s = state.read().await;
            (s.target_ra_hours, s.target_dec_degrees, s.target_pier_side)
        };
        if let (Some(target_ra), Some(target_dec), Some(params)) =
            (target_ra, target_dec, manager.parameters().await)
        {
            // ERFA refuses the host UTC if `eraCal2jd`
            // rejects the year (below `IYMIN = -4799`). A
            // leap-second-table-out-of-range clock returns
            // `Ok` with a warning, not an error — see the
            // `StarAdvError::Timekeeping` rustdoc — so the
            // realistic failure here is an absurdly-far-
            // past clock, not a future-shifted one. Match
            // the `poll_axes_now` failure pattern: log,
            // clear `slew_in_progress`, exit the watcher
            // rather than aborting the tokio task.
            let lst = match local_sidereal_time_hours(SystemTime::now(), config.site_longitude_deg)
            {
                Ok(lst) => lst,
                Err(e) => {
                    tracing::warn!("watcher LST computation failed: {e}");
                    return CompletionDecision::Bail;
                }
            };
            // Flip-aware: `encoder_to_celestial` applies the
            // post-flip RA/Dec mapping when the Dec encoder is
            // past the pole. Without it, the residual check
            // would interpret a successful flip as a 12-hour
            // RA residual and the pickup loop would try to undo
            // the flip on its first iteration.
            let (cur_ra, cur_dec) = encoder_to_celestial(
                RaTicks::new(snap.ra.position_ticks),
                DecTicks::new(snap.dec.position_ticks),
                lst,
                Cpr::new(params.cpr_ra),
                Cpr::new(params.cpr_dec),
                config.site_latitude_deg,
            );
            let (cur_ra, cur_dec) = (cur_ra.value(), cur_dec.value());
            // RA residual is on a 24-hour circle; take the
            // shorter arc. Convert hours → arc-seconds
            // (15°/hour × 3600″/°).
            let ra_circ =
                ((target_ra - cur_ra).rem_euclid(24.0)).min((cur_ra - target_ra).rem_euclid(24.0));
            let ra_residual_arcsec = ra_circ * 15.0 * 3600.0;
            let dec_residual_arcsec = (target_dec - cur_dec).abs() * 3600.0;
            if ra_residual_arcsec > PICKUP_TOLERANCE_ARCSEC
                || dec_residual_arcsec > PICKUP_TOLERANCE_ARCSEC
            {
                // Re-check the abort / disconnect signals
                // immediately before issuing any wire
                // commands. The top-of-loop guard ran one
                // `:f` round-trip + a few coordinate ops
                // ago; in that window AbortSlew (which
                // clears `slew_in_progress` and issues :L)
                // or set_connected(false) (which closes the
                // transport) may have raced ahead. Without
                // this second guard the pickup loop would
                // restart motion after the user aborted.
                if watcher_should_abort(&slew_in_progress, &session_slot).await {
                    return CompletionDecision::Bail;
                }
                // Pre-compensate the RA target for the LST drift
                // that will accumulate before the next pickup
                // iteration re-checks the residual. Without it
                // pickup chases a moving target and the residual
                // floor matches per-iteration sidereal drift
                // (~6″ on USB, ~14″ on UDP). See
                // `docs/plans/archive/star-adventurer-gti-pickup-accuracy.md`
                // §"Experiment B".
                //
                // Adaptive: use the actually-observed time delta
                // between consecutive pickup decisions; this
                // self-tunes for the transport's wire latency
                // (USB ≈ 400 ms/iter, UDP ≈ 950 ms/iter).
                // First iteration has no prior data → fall back
                // to `polling_interval × 2` (the USB-tuned heuristic).
                let now = std::time::Instant::now();
                let projection = match *last_pickup_at {
                    Some(t) => now.duration_since(t),
                    None => polling_interval.saturating_mul(2),
                };
                *last_pickup_at = Some(now);
                // Flip-aware target-encoder computation. With a
                // pre-flip target side, reuse `pickup_target_ra_ticks`
                // for the same LST pre-compensation that pre-Phase-6
                // builds relied on. With a post-flip target side,
                // compute the projected target via
                // `target_encoder_flipped` so the pickup re-slew
                // lands on the flipped encoder (past-the-pole Dec
                // and the mirror-band RA mech_HA) rather than
                // undoing the flip back to the pre-flip side.
                let pre_flip_side = pre_flip_side_for_latitude(config.site_latitude_deg);
                let target_is_flipped = target_pier_side
                    .as_ref()
                    .is_some_and(|s| *s != pre_flip_side && *s != PierSide::Unknown);
                let (new_ra_ticks, new_dec_ticks) = if target_is_flipped {
                    let lst_proj = Lst::new(lst.value() + projection.as_secs_f64() / 3600.0);
                    target_encoder_flipped(
                        Ra::new(target_ra),
                        Dec::new(target_dec),
                        lst_proj,
                        Cpr::new(params.cpr_ra),
                        Cpr::new(params.cpr_dec),
                    )
                } else {
                    let new_ra = pickup_target_ra_ticks(
                        Ra::new(target_ra),
                        lst,
                        projection,
                        Cpr::new(params.cpr_ra),
                    );
                    let new_dec = Dec::new(target_dec)
                        .to_mech()
                        .to_ticks(Cpr::new(params.cpr_dec));
                    (new_ra, new_dec)
                };
                // Fold the deltas to canonical so the pickup
                // re-slew takes the shortest path even if the
                // current encoder snapshot landed outside
                // `[−cpr/2, +cpr/2)` after a through-wrap
                // flip — see [`fold_to_canonical_band`].
                let ra_delta = new_ra_ticks
                    .canonical_delta_since(
                        RaTicks::new(snap.ra.position_ticks),
                        Cpr::new(params.cpr_ra),
                    )
                    .value();
                let dec_delta = new_dec_ticks
                    .canonical_delta_since(
                        DecTicks::new(snap.dec.position_ticks),
                        Cpr::new(params.cpr_dec),
                    )
                    .value();
                *pickup_iterations = pickup_iterations.saturating_add(1);
                debug!(
                    iteration = *pickup_iterations,
                    ra_residual_arcsec,
                    dec_residual_arcsec,
                    projection_ms = u64::try_from(projection.as_millis()).unwrap_or(u64::MAX),
                    ra_delta_ticks = ra_delta,
                    "slew pickup iteration"
                );
                // The pickup re-slew goes through the same
                // wire sequence as the original goto. `:L` +
                // poll keeps the motor-not-stopped contract
                // intact even if a previous send failed
                // mid-sequence.
                pickup_reslew_axis(&manager, session, Axis::Ra, ra_delta).await;
                pickup_reslew_axis(&manager, session, Axis::Dec, dec_delta).await;
                return CompletionDecision::Continue;
            }
        }
    }

    // Slew completed cleanly. Re-enable tracking if the user had
    // it on before the slew, then hand off to the helper's settle.
    // [`enable_sidereal_tracking_ra`] short-circuits on the first
    // failing send, so `tracking_requested = true` flips only when
    // all three wire commands succeed — otherwise `Tracking()`
    // would lie about the wire state. A single warn covers the
    // failure path (the failing-send error includes which command
    // failed).
    //
    // Re-check abort / disconnect before issuing the tracking
    // wire sequence — same race-window argument as the pickup
    // loop's pre-wire guard. AbortSlew clearing `slew_in_progress`
    // between the top-of-loop check and now must skip the
    // tracking restart, or the user-visible state would say
    // "aborted" while the wire is back to tracking.
    if watcher_should_abort(&slew_in_progress, &session_slot).await {
        return CompletionDecision::Bail;
    }
    if tracking_was_on {
        if let Some(params) = manager.parameters().await {
            match enable_sidereal_tracking_ra(&manager, session, &params).await {
                Ok(()) => {
                    state.write().await.tracking_requested = true;
                }
                Err(e) => {
                    tracing::warn!(
                        "post-slew tracking restart failed; tracking not re-enabled: {e}"
                    );
                }
            }
        }
    }
    CompletionDecision::Complete
}

/// Spawn the slew-completion watcher.
///
/// Polls the snapshot every `polling_interval` via
/// [`run_completion_watcher`]. When both axes report
/// `running == false` (or the slew was aborted externally — in
/// which case `slew_in_progress` is already cleared and the watcher
/// exits immediately), runs the EQMOD-style iterative pickup loop
/// to push any RA/Dec residual under [`PICKUP_TOLERANCE_ARCSEC`],
/// optionally re-issues sidereal tracking on the RA axis (matching
/// the design doc's "if Tracking was on" branch), waits `settle`,
/// then clears `slew_in_progress`.
///
/// `tracking_was_on` is captured at slew-issue time — the live
/// `tracking_requested` flag is cleared by `slew_to_coordinates_async`
/// so `tracking()` reports the wire state during the slew, hence we
/// can't read it from `state` here.
#[allow(clippy::too_many_arguments)]
pub(super) async fn spawn_slew_completion_watcher(
    state: Arc<RwLock<DriverState>>,
    manager: Arc<MountManager>,
    session_slot: SessionSlot,
    slew_in_progress: Arc<AtomicBool>,
    config: MountConfig,
    polling_interval: Duration,
    settle: Duration,
    tracking_was_on: bool,
) -> crate::error::Result<()> {
    // Acquire the watcher's own session BEFORE returning to the caller.
    // The watcher's session keeps the shared transport alive until the
    // watcher finishes, so the user's `set_connected(false)` can return
    // quickly (it closes the device's session, decrementing the
    // refcount to whatever active watchers hold). The user-disconnect
    // signal is the device's `session_slot.read().await.is_none()`,
    // independent of the refcount.
    let session = manager
        .transport()
        .acquire()
        .await
        .map_err(StarAdvError::from)?;
    let started = std::time::Instant::now();
    tokio::spawn(async move {
        let mut pickup_iterations: u32 = 0;
        // Adaptive pickup-target projection: track the instant of each
        // prior pickup re-slew so the next iteration can project the
        // residual target forward by *the actually-observed* iteration
        // duration rather than a hardcoded `polling_interval × 2`
        // multiplier. USB on the GTi sees ~400 ms per iteration; UDP
        // sees ~950 ms because the per-round-trip latency adds up
        // across the 5-frame re-slew sequence. The fixed multiplier
        // worked on USB but under-compensated on UDP by ~550 ms (~8″
        // of unaccounted LST drift per iteration). Measuring once a
        // prior iteration is available makes the projection self-tune
        // per transport.
        let mut last_pickup_at: Option<std::time::Instant> = None;
        let helper_state = Arc::clone(&state);
        let helper_manager = Arc::clone(&manager);
        let helper_slot = Arc::clone(&session_slot);
        let helper_flag = Arc::clone(&slew_in_progress);
        run_completion_watcher(
            helper_state,
            helper_manager,
            session,
            helper_slot,
            helper_flag,
            polling_interval,
            settle,
            "slew_watcher",
            // `async move` so the closure owns `state`, `manager`,
            // `session_slot`, `config`, `pickup_iterations` and
            // `last_pickup_at` outright. The step takes its
            // Arcs/config by value (cheap clones) so the returned
            // future doesn't borrow from the closure's captures.
            async move |snap, watcher_session| {
                slew_completion_step(
                    Arc::clone(&state),
                    Arc::clone(&manager),
                    Arc::clone(&session_slot),
                    Arc::clone(&slew_in_progress),
                    watcher_session,
                    config.clone(),
                    polling_interval,
                    started,
                    tracking_was_on,
                    &mut pickup_iterations,
                    &mut last_pickup_at,
                    snap,
                )
                .await
            },
            // Slew has no extra state to mutate at finalize time —
            // the helper clears `slew_in_progress` for us, and any
            // tracking restart was already done by
            // [`slew_completion_step`] under the polling-pause guard.
            |_s| {},
        )
        .await;
    });
    Ok(())
}

/// Spawn the park-completion watcher.
///
/// Same outer loop as [`spawn_slew_completion_watcher`] (provided
/// by [`run_completion_watcher`]), but the per-axes-stopped step
/// has no extra work to do, and the finalizer sets `at_park = true`
/// under the `DriverState` write lock just before the watcher clears
/// the `slew_in_progress` atomic. Park
/// always leaves tracking off per the ASCOM spec.
///
/// A blocked-axis abort (handled inside [`run_completion_watcher`])
/// does *not* set `at_park = true`: the OTA isn't at the encoder-0
/// home pose, so the next `Unpark + slew` would compute a wrong
/// delta. The helper enforces this by returning before the
/// finalizer runs.
pub(super) async fn spawn_park_completion_watcher(
    state: Arc<RwLock<DriverState>>,
    manager: Arc<MountManager>,
    session_slot: SessionSlot,
    slew_in_progress: Arc<AtomicBool>,
    polling_interval: Duration,
    settle: Duration,
) -> crate::error::Result<()> {
    let session = manager
        .transport()
        .acquire()
        .await
        .map_err(StarAdvError::from)?;
    tokio::spawn(async move {
        run_completion_watcher(
            state,
            manager,
            session,
            session_slot,
            slew_in_progress,
            polling_interval,
            settle,
            "park_watcher",
            async |_snap, _session| CompletionDecision::Complete,
            |s| {
                s.at_park = true;
            },
        )
        .await;
    });
    Ok(())
}

/// Spawn the `PulseGuide` watcher.
///
/// Sleeps for `duration`, then restores prior state on the targeted
/// axis:
/// - **RA pulse**: stop-and-wait, then if `tracking_was_on_for_restore`
///   re-issue `:G1 TRACKING` + `:I1 sidereal_period` + `:J1` so the
///   user-observable `Tracking` state survives the pulse.
/// - **Dec pulse**: stop-and-wait (Dec is normally idle; no restore).
///
/// The watcher checks the per-axis `pulse_guiding_<axis>` flag before
/// the restore step and bails out if cleared (the cancellation rule:
/// any axis-mutating call clears the flag before its own wire commands
/// so the watcher steps aside). Errors during the restore are logged
/// at `warn` and swallowed — matches [`pickup_reslew_axis`].
pub(super) async fn spawn_pulse_guide_watcher(
    state: Arc<RwLock<DriverState>>,
    manager: Arc<MountManager>,
    session_slot: SessionSlot,
    slew_in_progress: Arc<AtomicBool>,
    axis: Axis,
    duration: Duration,
    tracking_was_on_for_restore: bool,
) -> crate::error::Result<()> {
    // Acquire the watcher's own session up-front so the wait-for-`duration`
    // sleep doesn't have to navigate a transport that might disconnect
    // mid-pulse. The session keeps the shared transport alive until the
    // watcher closes it; the user-disconnect signal is the device's
    // `session_slot.read().await.is_none()`.
    let session = manager
        .transport()
        .acquire()
        .await
        .map_err(StarAdvError::from)?;
    tokio::spawn(async move {
        tokio::time::sleep(duration).await;
        // Bail if the pulse was cancelled externally (another op
        // cleared the flag), the user disconnected, or the mount
        // entered a state that takes ownership of the axis
        // (slew/park).
        let still_active = {
            let s = state.read().await;
            let active = if axis == Axis::Ra {
                s.pulse_guiding_ra
            } else {
                s.pulse_guiding_dec
            };
            active && !s.at_park
        } && !slew_in_progress.load(Ordering::SeqCst);
        if !still_active || user_disconnected(&session_slot).await {
            clear_pulse_flag(&state, axis).await;
            if let Err(e) = session.close().await {
                tracing::warn!(error = %e, "pulse-guide watcher session close failed");
            }
            return;
        }
        // Stop the axis. Any failure here means we can't safely restore
        // either, so log and bail.
        if let Err(e) = stop_axis_and_wait(&manager, &session, axis, AXIS_STOP_TIMEOUT).await {
            tracing::warn!("pulse-guide restore stop {axis:?} failed: {e}");
            clear_pulse_flag(&state, axis).await;
            if let Err(e) = session.close().await {
                tracing::warn!(error = %e, "pulse-guide watcher session close failed");
            }
            return;
        }
        // RA-only: re-issue sidereal tracking iff the user had it on
        // at issue time. Dec just stays stopped (Dec is normally idle).
        if axis == Axis::Ra && tracking_was_on_for_restore {
            // Re-check the cancellation flag before issuing the restore
            // commands — a concurrent set_tracking(false) between the
            // stop above and here would otherwise be silently undone.
            let still_want_restore = state.read().await.pulse_guiding_ra;
            if still_want_restore {
                if let Some(params) = manager.parameters().await {
                    if let Err(e) = enable_sidereal_tracking_ra(&manager, &session, &params).await {
                        tracing::warn!("pulse-guide tracking restore failed: {e}");
                    }
                }
            }
        }
        clear_pulse_flag(&state, axis).await;
        if let Err(e) = session.close().await {
            tracing::warn!(error = %e, "pulse-guide watcher session close failed");
        }
    });
    Ok(())
}
