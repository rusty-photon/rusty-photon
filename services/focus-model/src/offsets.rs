//! The offsets procedure: rounds of reference-then-filter sweeps, the
//! median of the differences, the writes and the restore.
//!
//! docs/services/focus-model.md § `determine_filter_offsets` is the
//! contract. Each sweep is the body `focus_train` runs, called here
//! rather than through `rp`: the procedure holds the provider's
//! one-run-at-a-time claim for its whole length, so a call that
//! reached its own tool through `rp` would wait on a claim it is
//! holding itself.

use std::collections::BTreeMap;

use serde::Serialize;
use tracing::{debug, warn};

use crate::config::Config;
use crate::error::{FocusModelError, Result};
use crate::sizing::plan_sweep;
use crate::store::{FocusRecord, FocusStore};
use crate::sweep::check_span;
use crate::workflow::{
    append_note, focus_one, lower_middle, model_label, record_for_write, resolve_train,
    stale_fields, within_travel, FocusRig, FocusTrainParams, Guiding, NoProgress, Progress, Rig,
    Session, TrainContext, GUIDING_NOT_RESUMED, RUN_NOT_RECORDED,
};

/// Rounds a call makes when it does not say.
const DEFAULT_ROUNDS: u32 = 2;

/// The most rounds a call may ask for. Each is a full pass of the
/// wheel, and the drift a median is meant to average out grows with
/// the time the procedure takes.
const MAX_ROUNDS: u32 = 5;

/// `determine_filter_offsets` arguments.
#[derive(Debug, Clone, Default)]
pub struct OffsetsParams {
    /// An `equipment.optical_trains[]` id.
    pub train_id: String,
    /// The filters to measure; default every name on the wheel.
    pub filters: Option<Vec<String>>,
    /// The filter the others are measured against.
    pub reference: Option<String>,
    /// How many times to walk the list; default 2, at most 5.
    pub rounds: Option<u32>,
}

/// One sweep of the procedure, as the result reports it.
#[derive(Debug, Clone, Serialize)]
pub struct OffsetSweep {
    /// The round this sweep belongs to, counting from 1.
    pub round: u32,
    /// The filter it focused through.
    pub filter: String,
    /// Whether the confirmation frame vouched for the vertex; only a
    /// confirmed sweep contributes a difference.
    pub confirmed: bool,
    /// Where the focuser was left, or null on a sweep that failed.
    pub position: Option<i32>,
    /// What was measured there.
    pub hfr: Option<f64>,
    /// Why the sweep gave nothing to difference; null when it focused.
    pub error: Option<String>,
    /// Why this sweep is missing from the run history; null when it
    /// was written. A sweep that focused and could not be recorded
    /// still measured what it measured — the difference stands — but
    /// the history does not have it. A sweep that *failed* and could
    /// not be recorded says so in `error` instead, after the failure
    /// itself.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub not_recorded: Option<String>,
}

/// A filter the procedure could not place, and why.
#[derive(Debug, Clone, Serialize)]
pub struct Unmeasured {
    /// The filter's name.
    pub filter: String,
    /// What stopped it being measured.
    pub why: String,
}

/// Where the call left the rig.
#[derive(Debug, Clone, Serialize)]
pub struct Restored {
    /// The filter put back in the path, null on a wheel that reported
    /// none when the call started.
    pub filter: Option<String>,
    /// Where the focuser was left.
    pub position: Option<i32>,
    /// Why the rig could not be put back; null when it was.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

/// What the store did with the offsets.
#[derive(Debug, Clone, Serialize)]
pub struct OffsetsRecorded {
    /// Whether the reference and the offsets reached the record.
    pub offsets_written: bool,
    /// The stored offsets this write dropped, because the reference
    /// they were differences against is no longer the reference.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub offsets_dropped: Vec<String>,
    /// Runs the record holds, the procedure's own included; null when
    /// the write failed and the count could not be read. The sweeps
    /// before it may well have been recorded.
    pub runs: Option<usize>,
    /// Why the write did not land.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

/// The `determine_filter_offsets` result.
#[derive(Debug, Clone, Serialize)]
pub struct OffsetsView {
    /// The train the procedure ran on.
    pub train_id: String,
    /// The filter every difference was taken against.
    pub reference: String,
    /// How many rounds ran.
    pub rounds: u32,
    /// Filter name to steps from the reference, which is itself 0, as
    /// the record holds them after this call: what it measured, plus
    /// anything it did not measure that the reference has not changed
    /// under. `unmeasured` names what this call could not place,
    /// whether or not an older offset for it survives here.
    pub offsets: BTreeMap<String, i32>,
    /// What each median was taken over, so a spread the median hid is
    /// still readable.
    pub differences: BTreeMap<String, Vec<i32>>,
    /// The filters that kept no offset.
    pub unmeasured: Vec<Unmeasured>,
    /// Every sweep the procedure ran, in the order it ran them.
    pub sweeps: Vec<OffsetSweep>,
    /// Where the call left the rig.
    pub restored: Restored,
    /// What the store did.
    pub recorded: OffsetsRecorded,
    /// How the record reads against the train now.
    pub model: String,
}

/// The arguments resolved against the train, before anything moves.
#[derive(Debug)]
struct Plan {
    /// The train the sweeps run on.
    ctx: TrainContext,
    /// The wheel the filters sit on.
    wheel: String,
    /// The filters to measure, in the order the rounds walk them.
    filters: Vec<String>,
    /// The filter the differences are taken against.
    reference: String,
    /// How many rounds to walk.
    rounds: u32,
    /// The identity fields the record no longer matched when the call
    /// arrived. The first sweep's write resets it, so by the time the
    /// offsets are written the record reads fresh and the reason would
    /// be lost.
    entering_stale: Vec<String>,
}

impl Plan {
    /// The order one round walks: the reference first, so every
    /// difference in the round is against a position measured inside
    /// it, then each other filter.
    fn order(&self) -> impl Iterator<Item = &String> {
        std::iter::once(&self.reference).chain(
            self.filters
                .iter()
                .filter(move |name| *name != &self.reference),
        )
    }

    /// How many sweeps the whole procedure will run.
    fn sweeps(&self) -> u32 {
        let filters = u32::try_from(self.filters.len()).unwrap_or(u32::MAX);
        self.rounds.saturating_mul(filters)
    }
}

/// Where the rig was when the call arrived.
#[derive(Debug)]
struct Started {
    /// The filter in the path, or none the wheel would name.
    filter: Option<String>,
    /// Where the focuser was.
    position: i32,
}

/// What the rounds measured.
#[derive(Default)]
struct Measured {
    /// Every sweep, in order.
    sweeps: Vec<OffsetSweep>,
    /// Per filter, one difference per round that placed it.
    differences: BTreeMap<String, Vec<i32>>,
    /// Whether any round's reference sweep confirmed.
    reference_placed: bool,
    /// Per filter, the confirmed pairs whose difference did not fit a
    /// focuser position, and so could not be used.
    discarded: BTreeMap<String, usize>,
}

impl Measured {
    /// Where one filter was last left by a sweep that measured a
    /// position, confirmed or not. This is the restore target: a
    /// fallback is the lowest sample the sweep accepted, which is a
    /// place a frame was taken and a better place to leave a focuser
    /// than where the call happened to find it. Differences are a
    /// stricter question and take confirmed positions only.
    fn last_measured(&self, filter: &str) -> Option<i32> {
        self.sweeps
            .iter()
            .rev()
            .filter(|sweep| sweep.filter == filter)
            .find_map(|sweep| sweep.position)
    }

    /// Why one filter kept no offset.
    fn why_unmeasured(&self, filter: &str) -> String {
        if self.discarded.contains_key(filter) {
            return format!(
                "'{filter}' confirmed, but no difference from the reference fits a \
                 focuser position"
            );
        }
        if self.reference_placed {
            format!("no round confirmed '{filter}' and the reference together")
        } else {
            "no round's reference sweep confirmed".to_owned()
        }
    }

    /// How many sweeps reached no run in the history, whether they
    /// focused or failed. A store that refused a write is named on
    /// the sweep that lost it, but a call that answers nothing has no
    /// sweeps to show, so the refusal carries the count instead.
    fn unrecorded(&self) -> usize {
        self.sweeps
            .iter()
            .filter(|sweep| {
                sweep.not_recorded.is_some()
                    || sweep
                        .error
                        .as_deref()
                        .is_some_and(|error| error.contains(RUN_NOT_RECORDED))
            })
            .count()
    }

    /// Why nothing was measured, in the words that fit what happened:
    /// the sweeps that came up short, or — when every one of them
    /// confirmed — the differences that would not fit a focuser
    /// position, which is the only other way to get here.
    fn shortfall(&self) -> String {
        let missed = self.sweeps.iter().filter(|sweep| !sweep.confirmed).count();
        let missing = match self.unrecorded() {
            0 => String::new(),
            unrecorded => format!(", and {unrecorded} of them reached no run in the history"),
        };
        if missed == 0 {
            let discarded = self
                .discarded
                .values()
                .fold(0_usize, |total, seen| total.saturating_add(*seen));
            return format!(
                "{discarded} confirmed {} produced no difference that fits a focuser \
                 position{missing}",
                if discarded == 1 { "pair" } else { "pairs" }
            );
        }
        format!(
            "{missed} of {} sweeps did not confirm{missing}",
            self.sweeps.len()
        )
    }
}

/// The `determine_filter_offsets` body.
///
/// # Errors
///
/// Returns the resolution errors of [`resolve_train`], a train without
/// a filter wheel or without a filter to measure against the
/// reference, an out-of-range `rounds`, a filter name the wheel does
/// not hold, a reference outside the list it would be measured
/// against, the error of a device that failed or a caller that
/// cancelled mid-procedure, and — when every sweep came up short — the
/// refusal to write an offset that was never measured.
pub async fn determine_filter_offsets(
    rig: Rig<'_>,
    store: &FocusStore,
    config: &Config,
    params: &OffsetsParams,
    progress: &dyn Progress,
) -> Result<OffsetsView> {
    let plan = resolve(rig.active, store, config, params).await?;
    let started = started_state(rig.active, &plan).await?;
    let session = Session { rig, store, config };
    let (measured, fatal) = run_rounds(session, &plan, progress).await;
    let offsets = offsets_from(&measured, &plan.reference);
    // Nothing to write: put the rig back and say what happened. The
    // caller may be gone, and the wheel is not left on whichever
    // filter the rounds swept last.
    if fatal.is_some() || offsets.len() <= 1 {
        let mut restored = restore(rig, &plan, &started, &measured).await;
        // A sweep that focused and then could not resume takes the one
        // exit with no put-back to undo its pause, and says so in its
        // error. That is the only way out still holding one — every
        // other sweep resumed its own — so it is the only case worth
        // a second attempt, and a blind retry would otherwise pulse a
        // guider this call never paused.
        if fatal
            .as_ref()
            .is_some_and(|error| error.tool_message().contains(GUIDING_NOT_RESUMED))
        {
            if let Err(error) = rig.cleanup.resume_guiding().await {
                note(
                    &mut restored,
                    format!("{GUIDING_NOT_RESUMED}: {}", error.tool_message()),
                );
            }
        }
        let error = fatal.unwrap_or_else(|| {
            FocusModelError::Workflow(format!(
                "no filter was measured against '{}': {}",
                plan.reference,
                measured.shortfall()
            ))
        });
        return Err(append_note(error, restored.error));
    }
    // Something was measured, so it goes to the record before the rig
    // is touched again: half an hour of sweeps must not be lost to a
    // put-back that hangs.
    let unmeasured = unmeasured(&plan, &offsets, &measured);
    let (offsets, recorded, model) = write_offsets(store, &plan, offsets).await;
    let restored = restore(rig, &plan, &started, &measured).await;
    Ok(OffsetsView {
        train_id: plan.ctx.train_id.clone(),
        reference: plan.reference.clone(),
        rounds: plan.rounds,
        unmeasured,
        offsets,
        differences: measured.differences,
        sweeps: measured.sweeps,
        restored,
        recorded,
        model,
    })
}

/// Resolve the train and check every argument against it.
async fn resolve(
    rig: &dyn FocusRig,
    store: &FocusStore,
    config: &Config,
    params: &OffsetsParams,
) -> Result<Plan> {
    let ctx = resolve_train(rig, &params.train_id).await?;
    let Some(wheel) = ctx.filter_wheel_id.clone() else {
        return Err(FocusModelError::Workflow(format!(
            "train '{}' has no filter wheel; an offset is a difference between filters",
            ctx.train_id
        )));
    };
    let rounds = params.rounds.unwrap_or(DEFAULT_ROUNDS);
    if rounds == 0 || rounds > MAX_ROUNDS {
        return Err(FocusModelError::Workflow(format!(
            "rounds must be between 1 and {MAX_ROUNDS}"
        )));
    }
    // One read for both the reference a call that named none inherits
    // and the staleness the first sweep is about to reset.
    let held = store.get(&ctx.train_id).await?;
    let filters = requested_filters(&ctx, params)?;
    let reference = reference_filter(&ctx, held.as_ref(), params, &filters)?;
    if filters.iter().all(|name| name == &reference) {
        return Err(FocusModelError::Workflow(format!(
            "train '{}' has no filter to measure against '{reference}'",
            ctx.train_id
        )));
    }
    let entering_stale = stale_fields(held.as_ref(), &ctx);
    // Every filter's sweep must be sizable before the first one moves.
    // Incomplete optics with no configured sweep is a configuration
    // fault, identical for every filter and recorded as no run at all,
    // so meeting it once per filter would report a procedure that
    // measured nothing instead of the sizing error that explains it.
    let train = config.train(&ctx.train_id);
    let usable = held.as_ref().filter(|_| entering_stale.is_empty());
    for filter in &filters {
        let plan = plan_sweep(
            &ctx.train_id,
            &ctx.optics,
            &train,
            &config.sweep,
            ctx.wavelength_of(Some(filter)),
            usable
                .and_then(|record| record.last_good_for(Some(filter)))
                .map(|entry| entry.hfr),
        )?;
        // And a width no sweep may walk is configuration too: it needs
        // no centre to know, so it is refused here rather than met as
        // one failed sweep per filter. What the focuser's bounds leave
        // of a grid does depend on where it is centred, and stays the
        // run's to find.
        check_span(plan.half_width, plan.step_size)
            .map_err(|failure| FocusModelError::Workflow(failure.to_string()))?;
    }
    Ok(Plan {
        ctx,
        wheel,
        filters,
        reference,
        rounds,
        entering_stale,
    })
}

/// The filters to measure: the argument when given, else the wheel's
/// own list. Every name is checked against the wheel, and a name given
/// twice is measured once.
fn requested_filters(ctx: &TrainContext, params: &OffsetsParams) -> Result<Vec<String>> {
    let names = params
        .filters
        .clone()
        .unwrap_or_else(|| ctx.filters.clone().unwrap_or_default());
    let mut wanted: Vec<String> = Vec::new();
    for name in names {
        ctx.check_filter(&name)?;
        if !wanted.iter().any(|held| held == &name) {
            wanted.push(name);
        }
    }
    if wanted.is_empty() {
        return Err(FocusModelError::Workflow(format!(
            "train '{}' has no filter to measure",
            ctx.train_id
        )));
    }
    Ok(wanted)
}

/// The reference: the argument, else the record's when the list holds
/// it, else the first of the list.
fn reference_filter(
    ctx: &TrainContext,
    held: Option<&FocusRecord>,
    params: &OffsetsParams,
    filters: &[String],
) -> Result<String> {
    let reference = match params.reference.clone() {
        Some(name) => name,
        None => default_reference(ctx, held, filters)?,
    };
    ctx.check_filter(&reference)?;
    if !filters.iter().any(|filter| filter == &reference) {
        return Err(FocusModelError::Workflow(format!(
            "reference '{reference}' is not in filters: {}",
            filters.join(", ")
        )));
    }
    Ok(reference)
}

/// The reference a call that named none gets: the record's when the
/// list still holds it, else the first of the list.
fn default_reference(
    ctx: &TrainContext,
    held: Option<&FocusRecord>,
    filters: &[String],
) -> Result<String> {
    held.and_then(|record| record.reference_filter.clone())
        .filter(|name| filters.iter().any(|filter| filter == name))
        .or_else(|| filters.first().cloned())
        .ok_or_else(|| {
            FocusModelError::Workflow(format!("train '{}' has no filter to measure", ctx.train_id))
        })
}

/// Read where the rig is, so the procedure can put it back there.
async fn started_state(rig: &dyn FocusRig, plan: &Plan) -> Result<Started> {
    let filter = rig.get_filter(&plan.wheel).await?;
    let position = rig.get_focuser_position(&plan.ctx.focuser_id).await?;
    // A focuser parked outside its configured travel is a fact about
    // the rig, not about a filter: every sweep would refuse it, and
    // the call would end saying its sweeps did not confirm instead of
    // naming the one thing that is wrong. It is refused here, once,
    // before anything moves — as a single sweep refuses it.
    within_travel(&position)?;
    Ok(Started {
        filter,
        position: position.position,
    })
}

/// Walk the rounds, collecting the sweeps and the differences. A fatal
/// error stops the walk and is handed back beside what was measured
/// before it, so the caller can still put the rig back.
async fn run_rounds(
    session: Session<'_>,
    plan: &Plan,
    progress: &dyn Progress,
) -> (Measured, Option<FocusModelError>) {
    let mut measured = Measured::default();
    let total = f64::from(plan.sweeps());
    let mut done: u32 = 0;
    for round in 1..=plan.rounds {
        let mut reference_at = None;
        for filter in plan.order() {
            let sweep = match one_sweep(session, plan, round, filter).await {
                Ok(sweep) => sweep,
                Err(error) => return (measured, Some(error)),
            };
            done = done.saturating_add(1);
            progress
                .tick(f64::from(done), Some(total), describe(&sweep, plan.rounds))
                .await;
            if filter == &plan.reference {
                reference_at = sweep.position.filter(|_| sweep.confirmed);
                measured.reference_placed |= reference_at.is_some();
            } else if let (Some(reference), Some(position)) = (reference_at, confirmed_at(&sweep)) {
                // Clamping here would write a rail as an offset and
                // move a focuser to it later, so a difference that
                // does not fit a focuser position is no difference.
                if let Some(difference) = position.checked_sub(reference) {
                    measured
                        .differences
                        .entry(filter.clone())
                        .or_default()
                        .push(difference);
                } else {
                    let seen = measured.discarded.entry(filter.clone()).or_default();
                    *seen = seen.saturating_add(1);
                    debug!(
                        filter,
                        position, reference, "the difference does not fit a focuser position"
                    );
                }
            }
            measured.sweeps.push(sweep);
        }
    }
    (measured, None)
}

/// Where a sweep confirmed, or nothing.
fn confirmed_at(sweep: &OffsetSweep) -> Option<i32> {
    sweep.position.filter(|_| sweep.confirmed)
}

/// One filter's sweep. A sweep that did not fit is this filter's loss
/// for this round and the procedure carries on. Everything else ends
/// it — a caller that went away, a rig that failed, a store that
/// could not be read — because each would meet every remaining sweep
/// the same way, and because a failure the procedure survived would be
/// reported as a measurement that came up short.
async fn one_sweep(
    session: Session<'_>,
    plan: &Plan,
    round: u32,
    filter: &str,
) -> Result<OffsetSweep> {
    debug!(train_id = %plan.ctx.train_id, round, filter, "sweeping for the offsets");
    let params = FocusTrainParams {
        train_id: plan.ctx.train_id.clone(),
        filter: Some(filter.to_owned()),
        shared: false,
    };
    match focus_one(session, &params, &NoProgress, Guiding::Own).await {
        Ok(outcome) => Ok(OffsetSweep {
            round,
            filter: filter.to_owned(),
            confirmed: outcome.confirmed,
            position: Some(outcome.position),
            hfr: Some(outcome.hfr),
            error: None,
            not_recorded: outcome.recorded.error,
        }),
        // `Sweep` is the sweep's own verdict on this filter: a fit
        // that did not hold, or a grid that cannot be walked around
        // where this filter's sweep would centre. Every other kind —
        // a train whose camera or wheel moved on between rounds
        // included — says the rig, the store or the caller cannot
        // carry the procedure, and each would meet the remaining
        // filters the same way.
        Err(error @ FocusModelError::Sweep(_)) => Ok(OffsetSweep {
            round,
            filter: filter.to_owned(),
            confirmed: false,
            position: None,
            hfr: None,
            error: Some(error.tool_message()),
            not_recorded: None,
        }),
        Err(error) => Err(error),
    }
}

/// The progress message for one finished sweep.
fn describe(sweep: &OffsetSweep, rounds: u32) -> String {
    let round = sweep.round;
    let filter = &sweep.filter;
    match (sweep.position, sweep.hfr, &sweep.error) {
        (Some(position), Some(hfr), _) if sweep.confirmed => {
            format!("round {round}/{rounds} {filter}: focused at {position}, HFR {hfr:.2}")
        }
        (Some(position), _, _) => {
            format!("round {round}/{rounds} {filter}: did not confirm, left at {position}")
        }
        (None, _, Some(error)) => format!("round {round}/{rounds} {filter}: {error}"),
        (None, _, None) => format!("round {round}/{rounds} {filter}: no position"),
    }
}

/// The offsets: the median of each filter's differences, the reference
/// at 0.
fn offsets_from(measured: &Measured, reference: &str) -> BTreeMap<String, i32> {
    let mut offsets = BTreeMap::new();
    offsets.insert(reference.to_owned(), 0);
    for (filter, differences) in &measured.differences {
        if let Some(median) = median(differences) {
            offsets.insert(filter.clone(), median);
        }
    }
    offsets
}

/// The median of a filter's differences: the middle one, or the mean
/// of the two middles rounded away from zero, an offset being whole
/// steps.
fn median(values: &[i32]) -> Option<i32> {
    let mut sorted = values.to_vec();
    sorted.sort_unstable();
    let lower = *sorted.get(lower_middle(sorted.len()))?;
    let upper = *sorted.get(sorted.len() / 2)?;
    if lower == upper {
        return Some(lower);
    }
    let sum = i64::from(lower).saturating_add(i64::from(upper));
    let away = if sum >= 0 { 1 } else { -1 };
    i32::try_from(sum.saturating_add(away).saturating_div(2)).ok()
}

/// The filters that kept no offset, each with why.
fn unmeasured(
    plan: &Plan,
    offsets: &BTreeMap<String, i32>,
    measured: &Measured,
) -> Vec<Unmeasured> {
    plan.filters
        .iter()
        .filter(|filter| !offsets.contains_key(*filter))
        .map(|filter| Unmeasured {
            filter: filter.clone(),
            why: measured.why_unmeasured(filter),
        })
        .collect()
}

/// Write the reference and the offsets onto the train's record, and
/// report how the record reads afterwards. A store that will not take
/// them is named in the result rather than raised: the sweeps
/// happened, and the offsets are in the answer either way.
///
/// A call that measured a subset of the wheel leaves the stored
/// offsets it did not measure alone — they are still differences
/// against the same reference. A call that changes the reference drops
/// them instead, because they are differences against a filter that no
/// longer is one, and names them.
async fn write_offsets(
    store: &FocusStore,
    plan: &Plan,
    offsets: BTreeMap<String, i32>,
) -> (BTreeMap<String, i32>, OffsetsRecorded, String) {
    let ctx = &plan.ctx;
    let reference = plan.reference.clone();
    let measured_offsets = offsets.clone();
    let reset = (!plan.entering_stale.is_empty())
        .then(|| format!("reset: {}", plan.entering_stale.join("; ")));
    let written = store
        .update(&ctx.train_id, move |held| {
            let stale = stale_fields(held.as_ref(), ctx);
            let (mut record, _) = record_for_write(held, &stale, ctx);
            let same_reference = record.reference_filter.as_deref() == Some(reference.as_str());
            let (mut kept, dropped) = if same_reference {
                (record.offsets.clone(), Vec::new())
            } else {
                let dropped = record
                    .offsets
                    .keys()
                    .filter(|name| !offsets.contains_key(*name))
                    .cloned()
                    .collect();
                (BTreeMap::new(), dropped)
            };
            kept.extend(offsets);
            record.set_offsets(Some(&reference), kept);
            let written = record.offsets.clone();
            Ok::<_, FocusModelError>((record, (stale, dropped, written)))
        })
        .await;
    match written {
        Ok((record, (stale, offsets_dropped, offsets))) => (
            offsets,
            OffsetsRecorded {
                offsets_written: true,
                offsets_dropped,
                runs: Some(record.run_count(None)),
                error: None,
            },
            reset.unwrap_or_else(|| model_label(Some(&record), &stale)),
        ),
        Err(error) => {
            warn!(train_id = %ctx.train_id, error = %error, "the offsets could not be recorded");
            (
                // The record would not take them, so the answer
                // carries what the call measured and nothing else.
                measured_offsets,
                OffsetsRecorded {
                    offsets_written: false,
                    offsets_dropped: Vec::new(),
                    runs: None,
                    error: Some(error.tool_message()),
                },
                "unrecorded".to_owned(),
            )
        }
    }
}

/// Put the wheel and the focuser back: the filter the call found in
/// the path, at that filter's measured position from the last round
/// that placed it, else where the focuser was when the call arrived.
/// Runs on the client a cancellation cannot reach.
async fn restore(rig: Rig<'_>, plan: &Plan, started: &Started, measured: &Measured) -> Restored {
    let mut restored = Restored {
        filter: started.filter.clone(),
        position: None,
        error: None,
    };
    if let Some(name) = &started.filter {
        if let Err(error) = rig.cleanup.set_filter(&plan.wheel, name).await {
            note(&mut restored, error.tool_message());
            // The wheel is wherever the last sweep left it, so naming
            // the call's own filter would pair the reported position
            // with the wrong one. Read it back, and report no filter
            // when even that fails.
            restored.filter = rig.cleanup.get_filter(&plan.wheel).await.ok().flatten();
        }
    }
    // The position follows the filter that is actually in the path.
    // Where the call found the focuser counts only while the path
    // holds the filter it was found under: after a wheel that would
    // not turn back, that position was measured through another
    // filter, and moving to it would be a restore in name only. A
    // wheel that named no filter at the start is the same case — the
    // rounds have since selected one, and there is nothing to put
    // back in the path.
    let restorable = started.filter.is_some() && restored.filter == started.filter;
    let target = restored
        .filter
        .as_deref()
        .and_then(|name| measured.last_measured(name))
        .or_else(|| restorable.then_some(started.position));
    if let Some(target) = target {
        let (reached, failed) = settle_at(rig.cleanup, &plan.ctx.focuser_id, target).await;
        restored.position = reached;
        if let Some(failed) = failed {
            note(&mut restored, failed);
            // The claim goes when this body ends, so a move `rp` gave
            // up on must not be handed to the next call still
            // travelling.
            note(
                &mut restored,
                wait_until_still(rig.cleanup, &plan.ctx.focuser_id).await,
            );
        }
    } else {
        // Nothing was moved here, but nothing proves the focuser is
        // idle either: a sweep `rp` abandoned mid-travel can still be
        // going. A reading is worth having and goes in the note; the
        // field that means a settled position stays null.
        if let Ok(read) = rig.cleanup.get_focuser_position(&plan.ctx.focuser_id).await {
            note(
                &mut restored,
                format!("the focuser read back at {}", read.position),
            );
        }
        let why = if started.filter.is_none() {
            "the wheel named no filter when the call started, so there was nothing to put \
             back in the path and the focuser was left where the last sweep put it"
                .to_owned()
        } else {
            restored.filter.as_ref().map_or_else(
                || {
                    "the wheel would not turn back and the filter it holds could not be \
                     read, so the focuser was left where the last sweep put it"
                        .to_owned()
                },
                |name| {
                    format!(
                        "the wheel holds '{name}', which this call measured nothing \
                         through, so the focuser was left where the last sweep put it"
                    )
                },
            )
        };
        note(&mut restored, why);
    }
    if let Some(error) = &restored.error {
        warn!(train_id = %plan.ctx.train_id, error, "the rig could not be put back");
    }
    restored
}

/// Move the focuser to `target` and prove it landed, trying once more
/// if it did not. `rp` answers a move whose deadline expired while the
/// device was idle with the position it actually reached, so an `Ok`
/// is not on its own a restore; and a move the last sweep abandoned
/// can still be travelling when this one lands.
async fn settle_at(
    rig: &dyn FocusRig,
    focuser_id: &str,
    target: i32,
) -> (Option<i32>, Option<String>) {
    let mut last = None;
    for _ in 0..2 {
        match rig.move_focuser(focuser_id, target).await {
            Ok(reached) if reached == target => return (Some(reached), None),
            Ok(reached) => last = Some(reached),
            // A move that failed may have travelled before it did,
            // and may still be travelling: `rp` answers a focuser it
            // gave up waiting for while `is_moving` is still true. So
            // there is no settled position to report — the reading is
            // worth having, but it goes in the note, not in a field
            // that means where the focuser was left.
            Err(error) => {
                let note = match rig.get_focuser_position(focuser_id).await {
                    Ok(read) => format!(
                        "{}; the focuser read back at {} and may still be moving",
                        error.tool_message(),
                        read.position
                    ),
                    Err(_) => error.tool_message(),
                };
                return (None, Some(note));
            }
        }
    }
    (
        last,
        last.map(|at| format!("the focuser settled at {at} instead of {target}")),
    )
}

/// Reads to take while waiting for a focuser to stop changing, and
/// the gap between them: ten seconds in all, twice `rp`'s own floor
/// on a move's deadline.
const SETTLE_READS: u32 = 20;
const SETTLE_POLL: std::time::Duration = std::time::Duration::from_millis(500);

/// Wait for the focuser to stop changing, and say how that went.
///
/// A move `rp` gave up on may still be travelling, and this provider
/// holds its one-focus-run claim until the tool body ends — so the
/// wait is what keeps the next call from reading a position mid-flight
/// and sweeping from it. Two agreeing reads is the only idleness
/// available: `rp` reports where a focuser is, not whether it is
/// moving. Stopping the travel is `rp`'s to do, not a caller's.
async fn wait_until_still(rig: &dyn FocusRig, focuser_id: &str) -> String {
    let mut last = None;
    for _ in 0..SETTLE_READS {
        let Ok(read) = rig.get_focuser_position(focuser_id).await else {
            return "the focuser could not be read while it settled".to_owned();
        };
        if last == Some(read.position) {
            return format!("the focuser came to rest at {}", read.position);
        }
        last = Some(read.position);
        tokio::time::sleep(SETTLE_POLL).await;
    }
    format!(
        "the focuser was still moving {}s later",
        SETTLE_POLL.saturating_mul(SETTLE_READS).as_secs()
    )
}

/// Add a note to the restore, keeping one already there.
fn note(restored: &mut Restored, note: String) {
    restored.error = Some(match restored.error.take() {
        Some(first) => format!("{first}; {note}"),
        None => note,
    });
}

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
mod tests {
    use std::sync::{Arc, Mutex};

    use super::*;
    use crate::sizing::Optics;
    use crate::store::LastGood;
    use crate::workflow::{
        CaptureResult, FocuserPosition, MockFocusRig, RefocusPlan, StarMeasurement, TrainInfo,
    };

    /// A train whose sweep is configured outright, so the tests need no
    /// optics: thirteen points at ±60, step 10 — wide enough that a
    /// filter's own focus is inside the grid the sweep before it left
    /// the focuser in.
    const CONFIGURED: &str = r#"{
        "mcp_server_url": "http://127.0.0.1:1/mcp",
        "trains": {
            "main": {
                "duration": "10ms", "step_size": 10, "half_width": 60,
                "min_fit_points": 3, "max_attempts": 1
            }
        }
    }"#;

    fn config() -> Config {
        crate::config::parse_config(CONFIGURED, "test").unwrap()
    }

    async fn temp_store() -> (FocusStore, tempfile::TempDir) {
        let dir = tempfile::tempdir().unwrap();
        let store = FocusStore::open(dir.path().join("focus.redb"))
            .await
            .unwrap();
        (store, dir)
    }

    fn filters() -> Vec<String> {
        ["Luminance", "Ha", "OIII"]
            .iter()
            .map(|name| (*name).to_owned())
            .collect()
    }

    /// The rig the procedure drives: a focuser it can move, a wheel it
    /// can turn, and a V per filter — a filter with no vertex measures
    /// starless frames, as a narrowband filter on a poor night does.
    #[derive(Clone)]
    struct Bench {
        position: Arc<Mutex<i32>>,
        filter: Arc<Mutex<String>>,
        vertices: Arc<BTreeMap<String, i32>>,
        /// Moves to answer before `move_focuser` starts answering with
        /// `ends`, and what it answers with then.
        budget: Arc<Mutex<Option<(u32, Ends)>>>,
        /// Whether `get_filter` names the filter in the path; a wheel
        /// between positions names none.
        names_filter: Arc<Mutex<bool>>,
        /// Whether the guiding train shares this focuser.
        guide_coupled: Arc<Mutex<bool>>,
        /// Resume calls the put-back client has answered, and whether
        /// the first of them fails.
        resumes: Arc<Mutex<u32>>,
        first_resume_fails: Arc<Mutex<bool>>,
    }

    /// How a focuser stops answering once its move budget runs out.
    #[derive(Clone, Copy)]
    enum Ends {
        /// The device failed.
        Failed,
        /// `rp` forwarded the caller's cancellation.
        Cancelled,
    }

    impl Bench {
        fn new(at: i32, vertices: &[(&str, i32)]) -> Self {
            Self {
                position: Arc::new(Mutex::new(at)),
                filter: Arc::new(Mutex::new("Luminance".to_owned())),
                vertices: Arc::new(
                    vertices
                        .iter()
                        .map(|(name, vertex)| ((*name).to_owned(), *vertex))
                        .collect(),
                ),
                budget: Arc::new(Mutex::new(None)),
                names_filter: Arc::new(Mutex::new(true)),
                guide_coupled: Arc::new(Mutex::new(false)),
                resumes: Arc::new(Mutex::new(0)),
                first_resume_fails: Arc::new(Mutex::new(false)),
            }
        }

        fn fails_moving_after(&self, moves: u32) {
            *self.budget.lock().unwrap() = Some((moves, Ends::Failed));
        }

        fn is_cancelled_after(&self, moves: u32) {
            *self.budget.lock().unwrap() = Some((moves, Ends::Cancelled));
        }

        /// A wheel that reports no filter in the path, as one between
        /// positions does.
        fn names_no_filter(&self) {
            *self.names_filter.lock().unwrap() = false;
        }

        /// A focuser the guiding train shares, whose first resume
        /// fails — the one case that leaves corrections paused with
        /// no put-back to undo it.
        fn couples_guiding_and_loses_the_first_resume(&self) {
            *self.guide_coupled.lock().unwrap() = true;
            *self.first_resume_fails.lock().unwrap() = true;
        }

        fn resumes(&self) -> u32 {
            *self.resumes.lock().unwrap()
        }

        fn at(&self) -> i32 {
            *self.position.lock().unwrap()
        }

        fn in_path(&self) -> String {
            self.filter.lock().unwrap().clone()
        }

        /// The active client: every read and write a sweep makes.
        fn rig(&self) -> MockFocusRig {
            let mut rig = MockFocusRig::new();
            rig.expect_get_train_info()
                .returning(|_| Box::pin(async { Ok(train_info(true)) }));
            let coupled = Arc::clone(&self.guide_coupled);
            rig.expect_get_refocus_plan().returning(move |_| {
                let guide_coupled = *coupled.lock().unwrap();
                Box::pin(async move {
                    Ok(RefocusPlan {
                        guide_coupled,
                        ..RefocusPlan::default()
                    })
                })
            });
            rig.expect_guiding_active()
                .returning(|| Box::pin(async { Ok(true) }));
            rig.expect_pause_guiding()
                .returning(|| Box::pin(async { Ok(()) }));
            rig.expect_get_focuser_temperature()
                .returning(|_| Box::pin(async { Ok(Some(11.0)) }));
            rig.expect_is_cancelled().returning(|| false);
            rig.expect_capture().returning(|_, _| {
                Box::pin(async {
                    Ok(CaptureResult {
                        document_id: "doc".to_owned(),
                    })
                })
            });
            self.wire_focuser(&mut rig);
            self.wire_wheel(&mut rig);
            self.wire_measurement(&mut rig);
            rig
        }

        /// The put-back client, which answers the same moves and reads
        /// — and keeps answering after the active one has stopped,
        /// because a cancellation does not reach it.
        fn cleanup(&self) -> MockFocusRig {
            let mut rig = MockFocusRig::new();
            self.wire_moves(&mut rig, &Arc::new(Mutex::new(None)));
            self.wire_wheel(&mut rig);
            let resumes = Arc::clone(&self.resumes);
            let loses_first = Arc::clone(&self.first_resume_fails);
            rig.expect_resume_guiding().returning(move || {
                let mut seen = resumes.lock().unwrap();
                *seen = seen.saturating_add(1);
                let lost = *seen == 1 && *loses_first.lock().unwrap();
                Box::pin(async move {
                    if lost {
                        return Err(FocusModelError::ToolCall(
                            "resume_guiding: the guider did not answer".to_owned(),
                        ));
                    }
                    Ok(())
                })
            });
            rig
        }

        fn wire_focuser(&self, rig: &mut MockFocusRig) {
            let budget = Arc::clone(&self.budget);
            self.wire_moves(rig, &budget);
        }

        fn wire_moves(&self, rig: &mut MockFocusRig, budget: &Arc<Mutex<Option<(u32, Ends)>>>) {
            let position = Arc::clone(&self.position);
            rig.expect_get_focuser_position().returning(move |_| {
                let position = *position.lock().unwrap();
                Box::pin(async move {
                    Ok(FocuserPosition {
                        position,
                        ..FocuserPosition::default()
                    })
                })
            });
            let position = Arc::clone(&self.position);
            let budget = Arc::clone(budget);
            rig.expect_move_focuser().returning(move |_, to| {
                let spent = {
                    let mut budget = budget.lock().unwrap();
                    match budget.as_mut() {
                        Some((0, ends)) => Some(*ends),
                        Some((left, _)) => {
                            *left -= 1;
                            None
                        }
                        None => None,
                    }
                };
                match spent {
                    Some(Ends::Failed) => {
                        return Box::pin(async {
                            Err(FocusModelError::ToolCall(
                                "move_focuser: the focuser stopped answering".to_owned(),
                            ))
                        })
                    }
                    Some(Ends::Cancelled) => {
                        return Box::pin(async {
                            Err(FocusModelError::Cancelled(
                                "the caller cancelled the focus run".to_owned(),
                            ))
                        })
                    }
                    None => {}
                }
                *position.lock().unwrap() = to;
                Box::pin(async move { Ok(to) })
            });
        }

        fn wire_wheel(&self, rig: &mut MockFocusRig) {
            let filter = Arc::clone(&self.filter);
            let names = Arc::clone(&self.names_filter);
            rig.expect_get_filter().returning(move |_| {
                let named = *names.lock().unwrap();
                let filter = filter.lock().unwrap().clone();
                Box::pin(async move { Ok(named.then_some(filter)) })
            });
            let filter = Arc::clone(&self.filter);
            rig.expect_set_filter().returning(move |_, name| {
                *filter.lock().unwrap() = name.to_owned();
                Box::pin(async { Ok(()) })
            });
        }

        fn wire_measurement(&self, rig: &mut MockFocusRig) {
            let position = Arc::clone(&self.position);
            let filter = Arc::clone(&self.filter);
            let vertices = Arc::clone(&self.vertices);
            rig.expect_measure_stars().returning(move |_, _, _, _| {
                let at = *position.lock().unwrap();
                let vertex = vertices.get(&*filter.lock().unwrap()).copied();
                Box::pin(async move {
                    Ok(match vertex {
                        Some(vertex) => {
                            let dx = f64::from(at - vertex);
                            StarMeasurement {
                                median_hfr: Some(1.0 + dx * dx / 400.0),
                                star_count: 100,
                            }
                        }
                        None => StarMeasurement {
                            median_hfr: None,
                            star_count: 0,
                        },
                    })
                })
            });
        }
    }

    fn train_info(wheel: bool) -> TrainInfo {
        TrainInfo {
            camera_id: Some("main-cam".to_owned()),
            filter_wheel_id: wheel.then(|| "main-fw".to_owned()),
            filters: wheel.then(filters),
            filter_wavelengths_nm: None,
            terminal_focuser_id: Some("main-focuser".to_owned()),
            optics: Optics::default(),
        }
    }

    /// A record the train's identity matches, holding offsets measured
    /// against `reference` on an earlier night.
    fn seeded(reference: &str) -> crate::store::FocusRecord {
        let mut record = crate::store::FocusRecord::new(
            "main",
            Some("main-focuser"),
            Some("main-cam"),
            Some(filters()),
        );
        record.set_offsets(
            Some(reference),
            [("Ha".to_owned(), 46), ("OIII".to_owned(), -20)].into(),
        );
        record
    }

    fn params(rounds: u32) -> OffsetsParams {
        OffsetsParams {
            train_id: "main".to_owned(),
            rounds: Some(rounds),
            ..OffsetsParams::default()
        }
    }

    async fn run(bench: &Bench, store: &FocusStore, params: &OffsetsParams) -> Result<OffsetsView> {
        let active = bench.rig();
        let cleanup = bench.cleanup();
        determine_filter_offsets(
            Rig {
                active: &active,
                cleanup: &cleanup,
            },
            store,
            &config(),
            params,
            &NoProgress,
        )
        .await
    }

    // --- the median -----------------------------------------------------

    /// Two rounds is the default, so the even split is the common case:
    /// the two differences are averaged, and a half step goes away from
    /// zero rather than towards it, an offset being whole steps.
    #[test]
    fn the_median_of_an_even_split_is_the_mean_rounded_away_from_zero() {
        assert_eq!(median(&[45, 46]), Some(46));
        assert_eq!(median(&[-45, -46]), Some(-46));
        assert_eq!(median(&[45, 47]), Some(46));
        assert_eq!(median(&[46, 46]), Some(46));
    }

    #[test]
    fn an_odd_count_takes_the_middle_difference() {
        assert_eq!(median(&[10]), Some(10));
        assert_eq!(median(&[3, 1, 2]), Some(2));
        assert_eq!(median(&[-9, 100, 4, 5, 6]), Some(5));
        assert_eq!(median(&[]), None);
    }

    // --- the procedure --------------------------------------------------

    /// Each round focuses the reference and then each other filter, and
    /// every difference is against the reference position measured
    /// inside that round.
    #[tokio::test]
    async fn every_filter_is_measured_against_the_reference_each_round() {
        let (store, _dir) = temp_store().await;
        let bench = Bench::new(
            25_000,
            &[("Luminance", 25_000), ("Ha", 25_030), ("OIII", 24_980)],
        );

        let view = run(&bench, &store, &params(2)).await.unwrap();

        assert_eq!(view.reference, "Luminance");
        assert_eq!(view.offsets.get("Luminance"), Some(&0));
        assert_eq!(view.offsets.get("Ha"), Some(&30));
        assert_eq!(view.offsets.get("OIII"), Some(&-20));
        assert_eq!(view.differences.get("Ha"), Some(&vec![30, 30]));
        assert_eq!(view.sweeps.len(), 6, "three filters, twice");
        assert!(view.unmeasured.is_empty(), "{:?}", view.unmeasured);
        assert!(view.recorded.offsets_written);
        assert_eq!(view.recorded.runs, Some(6));
        assert_eq!(view.model, "fresh");

        let record = store.get("main").await.unwrap().unwrap();
        assert_eq!(record.reference_filter.as_deref(), Some("Luminance"));
        assert_eq!(record.offset_for(Some("Ha")), Some(30));
        assert_eq!(record.run_count(None), 6);
    }

    /// The wheel goes back to the filter the call found in the path,
    /// and the focuser to that filter's own measured position — a place
    /// a sweep found, not one the offsets computed.
    #[tokio::test]
    async fn the_rig_goes_back_to_the_filter_the_call_found() {
        let (store, _dir) = temp_store().await;
        let bench = Bench::new(
            25_000,
            &[("Luminance", 25_000), ("Ha", 25_030), ("OIII", 24_980)],
        );
        *bench.filter.lock().unwrap() = "Ha".to_owned();

        let view = run(&bench, &store, &params(1)).await.unwrap();

        assert_eq!(view.restored.filter.as_deref(), Some("Ha"));
        assert_eq!(view.restored.position, Some(25_030));
        assert_eq!(view.restored.error, None);
        assert_eq!(bench.in_path(), "Ha");
        assert_eq!(bench.at(), 25_030);
    }

    /// A wheel that names no filter at the start has nothing to put
    /// back in the path: the rounds have selected one since, and
    /// reporting the starting position beside a null filter would pair
    /// a position with a path that was never restored.
    #[tokio::test]
    async fn a_wheel_that_named_no_filter_is_not_reported_as_restored() {
        let (store, _dir) = temp_store().await;
        let bench = Bench::new(
            25_000,
            &[("Luminance", 25_000), ("Ha", 25_030), ("OIII", 24_980)],
        );
        bench.names_no_filter();

        let view = run(&bench, &store, &params(1)).await.unwrap();

        assert_eq!(view.restored.filter, None);
        assert!(
            view.restored
                .error
                .as_deref()
                .unwrap_or_default()
                .contains("named no filter when the call started"),
            "{:?}",
            view.restored
        );
        assert_eq!(
            bench.at(),
            24_980,
            "the focuser stays where the last sweep put it"
        );
    }

    /// A filter whose frames carry no stars keeps no offset and is
    /// named with why; the filters that did confirm are still written.
    #[tokio::test]
    async fn a_filter_that_never_confirms_is_named_rather_than_guessed() {
        let (store, _dir) = temp_store().await;
        let bench = Bench::new(25_000, &[("Luminance", 25_000), ("Ha", 25_030)]);

        let view = run(&bench, &store, &params(1)).await.unwrap();

        assert_eq!(view.offsets.get("Ha"), Some(&30));
        assert!(!view.offsets.contains_key("OIII"));
        assert_eq!(view.unmeasured.len(), 1);
        let unmeasured = view.unmeasured.first().unwrap();
        assert_eq!(unmeasured.filter, "OIII");
        assert_eq!(
            unmeasured.why,
            "no round confirmed 'OIII' and the reference together"
        );
        let starless = view
            .sweeps
            .iter()
            .find(|sweep| sweep.filter == "OIII")
            .unwrap();
        assert!(!starless.confirmed);
        assert!(
            starless.error.as_deref().unwrap().contains("not enough"),
            "{starless:?}"
        );
    }

    /// A round whose reference sweep did not confirm has nothing to
    /// difference against, so it contributes nothing at all — and when
    /// no round does, the call refuses to write rather than inventing
    /// an offset.
    #[tokio::test]
    async fn a_reference_that_never_confirms_leaves_nothing_to_write() {
        let (store, _dir) = temp_store().await;
        let bench = Bench::new(25_000, &[("Ha", 25_030), ("OIII", 24_980)]);

        let err = run(&bench, &store, &params(1)).await.unwrap_err();

        assert_eq!(
            err.tool_message(),
            "no filter was measured against 'Luminance': 1 of 3 sweeps did not confirm"
        );
        let record = store.get("main").await.unwrap().unwrap();
        assert_eq!(record.reference_filter, None, "nothing was written");
        assert_eq!(record.run_count(None), 3, "every sweep is still recorded");
    }

    /// A device that failed would meet every remaining sweep the same
    /// way, so the procedure stops there — and still puts the rig back.
    #[tokio::test]
    async fn a_failed_device_ends_the_procedure() {
        let (store, _dir) = temp_store().await;
        let bench = Bench::new(25_000, &[("Luminance", 25_000), ("Ha", 25_030)]);
        // Enough moves for the first sweep and its confirmation, then
        // the focuser stops answering part way through the second.
        bench.fails_moving_after(20);

        let err = run(&bench, &store, &params(2)).await.unwrap_err();

        assert!(
            err.tool_message().contains("the focuser stopped answering"),
            "{err}"
        );
        let record = store.get("main").await.unwrap().unwrap();
        assert!(
            record.run_count(None) < 6,
            "the procedure stopped rather than walking every round"
        );
    }

    /// A call that measures part of the wheel leaves the rest of the
    /// stored offsets alone: they are still differences against the
    /// same reference, and this call measured nothing that contradicts
    /// them.
    #[tokio::test]
    async fn a_subset_call_keeps_the_offsets_it_did_not_measure() {
        let (store, _dir) = temp_store().await;
        store.put(seeded("Luminance")).await.unwrap();
        let bench = Bench::new(25_000, &[("Luminance", 25_000), ("Ha", 25_030)]);
        let subset = OffsetsParams {
            filters: Some(vec!["Luminance".to_owned(), "Ha".to_owned()]),
            ..params(1)
        };

        let view = run(&bench, &store, &subset).await.unwrap();

        assert!(view.recorded.offsets_dropped.is_empty());
        let record = store.get("main").await.unwrap().unwrap();
        assert_eq!(record.offset_for(Some("Ha")), Some(30), "measured again");
        assert_eq!(record.offset_for(Some("OIII")), Some(-20), "left alone");
    }

    /// A filter this call could not place keeps whatever the record
    /// held for it: an offset measured on an earlier night is better
    /// than none, and tonight's clouds do not unmeasure it. The
    /// result says both things — the offset the record carries, and
    /// that this call measured nothing for that filter.
    #[tokio::test]
    async fn a_filter_this_call_could_not_place_keeps_what_the_record_held() {
        let (store, _dir) = temp_store().await;
        store.put(seeded("Luminance")).await.unwrap();
        // OIII has no vertex, so its frames carry no stars.
        let bench = Bench::new(25_000, &[("Luminance", 25_000), ("Ha", 25_030)]);

        let view = run(&bench, &store, &params(1)).await.unwrap();

        assert_eq!(view.offsets.get("Ha"), Some(&30), "measured again");
        assert_eq!(
            view.offsets.get("OIII"),
            Some(&-20),
            "the record's, and this call took nothing away"
        );
        assert_eq!(view.unmeasured.len(), 1);
        assert_eq!(view.unmeasured.first().unwrap().filter, "OIII");
    }

    /// Changing the reference invalidates what the old offsets were
    /// differences against, so they go rather than being carried onto a
    /// filter they were never measured against — and they are named.
    #[tokio::test]
    async fn changing_the_reference_drops_the_offsets_it_invalidates() {
        let (store, _dir) = temp_store().await;
        store.put(seeded("Luminance")).await.unwrap();
        let bench = Bench::new(25_030, &[("Luminance", 25_000), ("Ha", 25_030)]);
        let rebased = OffsetsParams {
            filters: Some(vec!["Ha".to_owned(), "Luminance".to_owned()]),
            reference: Some("Ha".to_owned()),
            ..params(1)
        };

        let view = run(&bench, &store, &rebased).await.unwrap();

        assert_eq!(view.recorded.offsets_dropped, vec!["OIII".to_owned()]);
        let record = store.get("main").await.unwrap().unwrap();
        assert_eq!(record.reference_filter.as_deref(), Some("Ha"));
        assert_eq!(record.offset_for(Some("Luminance")), Some(-30));
        assert_eq!(record.offset_for(Some("OIII")), None);
    }

    /// A put-back that did not land leaves the focuser somewhere the
    /// next sweep would measure from, so it ends the procedure like any
    /// other device failure — and the caller is told, rather than
    /// reading it in the log.
    #[tokio::test]
    async fn a_put_back_that_did_not_land_ends_the_procedure() {
        let (store, _dir) = temp_store().await;
        let bench = Bench::new(25_000, &[]);
        let active = bench.rig();
        let mut cleanup = MockFocusRig::new();
        cleanup
            .expect_get_filter()
            .returning(|_| Box::pin(async { Ok(Some("Luminance".to_owned())) }));
        cleanup
            .expect_set_filter()
            .returning(|_, _| Box::pin(async { Ok(()) }));
        cleanup.expect_get_focuser_position().returning(|_| {
            Box::pin(async {
                Ok(FocuserPosition {
                    position: 25_000,
                    ..FocuserPosition::default()
                })
            })
        });
        cleanup.expect_move_focuser().returning(|_, _| {
            Box::pin(async {
                Err(FocusModelError::ToolCall(
                    "move_focuser: the focuser is not responding".to_owned(),
                ))
            })
        });

        let err = determine_filter_offsets(
            Rig {
                active: &active,
                cleanup: &cleanup,
            },
            &store,
            &config(),
            &params(1),
            &NoProgress,
        )
        .await
        .unwrap_err();

        assert!(
            err.tool_message().contains("the focuser is not responding"),
            "{err}"
        );
        let record = store.get("main").await.unwrap().unwrap();
        assert_eq!(
            record.run_count(None),
            1,
            "the procedure stopped at the first sweep"
        );
    }

    /// A cancellation reaches the procedure the way it reaches a
    /// sweep, through the `rp` call in flight. It stops the rounds
    /// where they are, leaves the record without an offsets write, and
    /// still puts the rig back — the restore runs on the client the
    /// cancellation cannot reach.
    #[tokio::test]
    async fn a_cancelled_sweep_stops_the_rounds_and_still_puts_the_rig_back() {
        let (store, _dir) = temp_store().await;
        let bench = Bench::new(
            25_000,
            &[("Luminance", 25_000), ("Ha", 25_030), ("OIII", 24_980)],
        );
        // Through the first sweep and its confirmation, then part way
        // into the second.
        bench.is_cancelled_after(20);

        let err = run(&bench, &store, &params(2)).await.unwrap_err();

        assert!(err.is_cancelled(), "{err}");
        let record = store.get("main").await.unwrap().unwrap();
        assert_eq!(record.reference_filter, None, "no offsets were written");
        assert!(
            record.run_count(None) < 6,
            "the rounds stopped where the cancellation landed"
        );
        assert_eq!(bench.in_path(), "Luminance", "the wheel went back");
        assert_eq!(bench.at(), 25_000, "and the focuser with it");
    }

    /// A record trained on another camera is reset by the first sweep,
    /// and the result still says so: by the time the offsets are
    /// written the record reads fresh, and reporting "fresh" would
    /// hide the reset this very call caused.
    #[tokio::test]
    async fn the_result_reports_the_reset_the_procedure_caused() {
        let (store, _dir) = temp_store().await;
        let record = crate::store::FocusRecord::new(
            "main",
            Some("main-focuser"),
            Some("retired-cam"),
            Some(filters()),
        );
        store.put(record).await.unwrap();
        let bench = Bench::new(
            25_000,
            &[("Luminance", 25_000), ("Ha", 25_030), ("OIII", 24_980)],
        );

        let view = run(&bench, &store, &params(1)).await.unwrap();

        assert_eq!(
            view.model,
            "reset: camera_id changed from retired-cam to main-cam"
        );
    }

    /// A sweep that fell back measured a position: the lowest sample it
    /// accepted. That is where the focuser is left when its filter goes
    /// back in the path, rather than the place the call started from —
    /// which was measured through some other filter, or not at all. The
    /// differences stay stricter and take confirmed positions only.
    #[test]
    fn a_fallback_position_is_still_where_the_focuser_is_left() {
        let sweep = |filter: &str, confirmed, position| OffsetSweep {
            round: 1,
            filter: filter.to_owned(),
            confirmed,
            position: Some(position),
            hfr: Some(1.0),
            error: None,
            not_recorded: None,
        };
        let measured = Measured {
            sweeps: vec![
                sweep("Luminance", true, 25_000),
                sweep("Ha", false, 25_030),
                sweep("OIII", true, 24_980),
            ],
            ..Measured::default()
        };

        assert_eq!(measured.last_measured("Ha"), Some(25_030));
        assert_eq!(measured.last_measured("Luminance"), Some(25_000));
        assert_eq!(measured.last_measured("SII"), None);
    }

    /// A run the store refused is counted whichever way its sweep
    /// ended: one that focused says so in `not_recorded`, one that
    /// failed carries the marker in the error it already had. A call
    /// that measured nothing has no result to show either on, so the
    /// refusal carries the count.
    #[test]
    fn a_run_the_store_refused_is_counted_whichever_way_the_sweep_ended() {
        let sweep = |confirmed, position, error, not_recorded: Option<&str>| OffsetSweep {
            round: 1,
            filter: "Ha".to_owned(),
            confirmed,
            position,
            hfr: position.map(|_| 1.0),
            error,
            not_recorded: not_recorded.map(str::to_owned),
        };
        let measured = Measured {
            sweeps: vec![
                sweep(
                    true,
                    Some(25_030),
                    None,
                    Some("the run could not be recorded: the store is full"),
                ),
                sweep(
                    false,
                    None,
                    Some(format!(
                        "not enough stars: 2 of 9 samples passed the gate; \
                         {RUN_NOT_RECORDED}: the store is full"
                    )),
                    None,
                ),
                sweep(true, Some(25_031), None, None),
            ],
            ..Measured::default()
        };

        assert_eq!(measured.unrecorded(), 2);
        assert_eq!(
            measured.shortfall(),
            "1 of 3 sweeps did not confirm, and 2 of them reached no run in the history"
        );

        let recorded = Measured {
            sweeps: vec![sweep(
                false,
                None,
                Some("not enough stars".to_owned()),
                None,
            )],
            ..Measured::default()
        };
        assert_eq!(recorded.unrecorded(), 0);
        assert_eq!(recorded.shortfall(), "1 of 1 sweeps did not confirm");
    }

    /// A filter that confirmed and still kept no offset did not fail to
    /// measure: its difference would not fit a focuser position, and
    /// saying it never confirmed would contradict its own sweep.
    #[test]
    fn a_discarded_difference_is_its_own_reason() {
        let mut measured = Measured {
            reference_placed: true,
            ..Measured::default()
        };
        measured.discarded.insert("Ha".to_owned(), 1);
        assert_eq!(
            measured.why_unmeasured("Ha"),
            "'Ha' confirmed, but no difference from the reference fits a focuser position"
        );
        assert_eq!(
            measured.why_unmeasured("OIII"),
            "no round confirmed 'OIII' and the reference together"
        );
        assert_eq!(
            Measured::default().why_unmeasured("Ha"),
            "no round's reference sweep confirmed"
        );
    }

    /// A sweep that focused and then could not resume guiding ends the
    /// procedure without a put-back to undo its own pause — that path
    /// leaves corrections paused, and it is the one the procedure has
    /// to clean up after. It tries the resume once more on its way
    /// out, on the client a cancellation cannot reach.
    #[tokio::test]
    async fn a_procedure_that_died_holding_a_pause_resumes_on_its_way_out() {
        let (store, _dir) = temp_store().await;
        let bench = Bench::new(25_000, &[("Luminance", 25_000), ("Ha", 25_030)]);
        bench.couples_guiding_and_loses_the_first_resume();

        let err = run(&bench, &store, &params(1)).await.unwrap_err();

        assert!(
            err.tool_message().contains("guiding could not be resumed"),
            "{err}"
        );
        assert_eq!(
            bench.resumes(),
            2,
            "the sweep's own resume, then the procedure's on the way out"
        );
    }

    /// The measurements are written before the rig is touched again,
    /// so a put-back that fails costs the rig's position and nothing
    /// else: the offsets are in the record and in the answer, with
    /// the failure named beside them.
    #[tokio::test]
    async fn a_rig_that_will_not_go_back_still_answers_with_the_offsets() {
        let (store, _dir) = temp_store().await;
        let bench = Bench::new(
            25_000,
            &[("Luminance", 25_000), ("Ha", 25_030), ("OIII", 24_980)],
        );
        let active = bench.rig();
        let mut cleanup = MockFocusRig::new();
        cleanup
            .expect_set_filter()
            .returning(|_, _| Box::pin(async { Ok(()) }));
        cleanup
            .expect_get_filter()
            .returning(|_| Box::pin(async { Ok(Some("Luminance".to_owned())) }));
        cleanup.expect_move_focuser().returning(|_, _| {
            Box::pin(async {
                Err(FocusModelError::ToolCall(
                    "move_focuser: the focuser is not responding".to_owned(),
                ))
            })
        });
        cleanup.expect_get_focuser_position().returning(|_| {
            Box::pin(async {
                Ok(FocuserPosition {
                    position: 24_980,
                    ..FocuserPosition::default()
                })
            })
        });

        let view = determine_filter_offsets(
            Rig {
                active: &active,
                cleanup: &cleanup,
            },
            &store,
            &config(),
            &params(1),
            &NoProgress,
        )
        .await
        .unwrap();

        assert!(view.recorded.offsets_written);
        assert_eq!(view.offsets.get("Ha"), Some(&30));
        assert!(
            view.restored
                .error
                .as_deref()
                .unwrap_or_default()
                .contains("not responding"),
            "{:?}",
            view.restored
        );
        let record = store.get("main").await.unwrap().unwrap();
        assert_eq!(record.offset_for(Some("Ha")), Some(30));
    }

    /// Two rounds is what a call that does not say gets — one round
    /// cannot average out the drift it is there to average — and zero
    /// is refused like any other number outside the range.
    #[tokio::test]
    async fn rounds_default_to_two_and_zero_is_refused() {
        let (store, _dir) = temp_store().await;
        let bench = Bench::new(
            25_000,
            &[("Luminance", 25_000), ("Ha", 25_030), ("OIII", 24_980)],
        );
        let unsaid = OffsetsParams {
            train_id: "main".to_owned(),
            ..OffsetsParams::default()
        };

        let view = run(&bench, &store, &unsaid).await.unwrap();

        assert_eq!(view.rounds, 2);
        assert_eq!(view.differences.get("Ha").map(Vec::len), Some(2));

        let zero = OffsetsParams {
            rounds: Some(0),
            ..params(1)
        };
        let rig = bench.rig();
        assert_eq!(
            resolve(&rig, &store, &config(), &zero)
                .await
                .unwrap_err()
                .tool_message(),
            "rounds must be between 1 and 5"
        );
    }

    // --- the arguments --------------------------------------------------

    #[tokio::test]
    async fn a_train_without_a_wheel_has_no_difference_to_measure() {
        let (store, _dir) = temp_store().await;
        let mut rig = MockFocusRig::new();
        rig.expect_get_train_info()
            .returning(|_| Box::pin(async { Ok(train_info(false)) }));

        let err = resolve(&rig, &store, &config(), &params(1))
            .await
            .unwrap_err();

        assert_eq!(
            err.tool_message(),
            "train 'main' has no filter wheel; an offset is a difference between filters"
        );
    }

    /// A grid wider than the cap a sweep may walk needs no centre to
    /// know it cannot be walked, so the call says so before the first
    /// filter rather than meeting it once per filter.
    #[tokio::test]
    async fn a_grid_wider_than_the_cap_is_refused_up_front() {
        let (store, _dir) = temp_store().await;
        let bench = Bench::new(25_000, &[("Luminance", 25_000)]);
        let rig = bench.rig();
        let wide = crate::config::parse_config(
            r#"{
                "mcp_server_url": "http://127.0.0.1:1/mcp",
                "trains": {
                    "main": {
                        "duration": "10ms", "step_size": 1, "half_width": 5000,
                        "min_fit_points": 3, "max_attempts": 1
                    }
                }
            }"#,
            "test",
        )
        .unwrap();

        let err = resolve(&rig, &store, &wide, &params(1)).await.unwrap_err();

        assert!(
            err.tool_message().contains("more than the cap of 1000"),
            "{err}"
        );
        assert_eq!(bench.at(), 25_000, "nothing moved");
    }

    /// A focuser parked outside its configured travel is a fact about
    /// the rig: every sweep would refuse it, so the call says so once
    /// rather than ending on a count of sweeps that did not confirm.
    #[tokio::test]
    async fn a_focuser_outside_its_travel_is_refused_before_the_rounds() {
        let (store, _dir) = temp_store().await;
        // The reads `resolve` and `started_state` make, and nothing
        // else: a refusal this early touches no device.
        let mut rig = MockFocusRig::new();
        rig.expect_get_train_info()
            .returning(|_| Box::pin(async { Ok(train_info(true)) }));
        rig.expect_get_refocus_plan()
            .returning(|_| Box::pin(async { Ok(RefocusPlan::default()) }));
        rig.expect_get_filter()
            .returning(|_| Box::pin(async { Ok(Some("Luminance".to_owned())) }));
        rig.expect_get_focuser_position().returning(|_| {
            Box::pin(async {
                Ok(FocuserPosition {
                    position: 61_000,
                    min_position: Some(0),
                    max_position: Some(60_000),
                    backlash: None,
                })
            })
        });
        let plan = resolve(&rig, &store, &config(), &params(1)).await.unwrap();

        let err = started_state(&rig, &plan).await.unwrap_err();

        assert_eq!(
            err.tool_message(),
            "the focuser is at 61000, outside its configured travel [0, 60000]; nothing was moved"
        );
    }

    /// Optics the sweep cannot be sized from is a configuration fault,
    /// identical for every filter and recorded as no run at all.
    /// Meeting it once per filter would report a procedure that
    /// measured nothing; it is named once, before the first move.
    #[tokio::test]
    async fn optics_the_sweep_cannot_be_sized_from_are_refused_up_front() {
        let (store, _dir) = temp_store().await;
        let bench = Bench::new(25_000, &[("Luminance", 25_000)]);
        let rig = bench.rig();
        let bare = crate::config::parse_config(
            r#"{ "mcp_server_url": "http://127.0.0.1:1/mcp" }"#,
            "test",
        )
        .unwrap();

        let err = resolve(&rig, &store, &bare, &params(1)).await.unwrap_err();

        assert!(err.tool_message().contains("has no derived sweep"), "{err}");
        assert_eq!(bench.at(), 25_000, "nothing moved");
    }

    #[tokio::test]
    async fn the_arguments_are_refused_before_anything_moves() {
        let (store, _dir) = temp_store().await;
        let bench = Bench::new(25_000, &[("Luminance", 25_000)]);
        let rig = bench.rig();

        let out_of_range = OffsetsParams {
            rounds: Some(6),
            ..params(1)
        };
        assert_eq!(
            resolve(&rig, &store, &config(), &out_of_range)
                .await
                .unwrap_err()
                .tool_message(),
            "rounds must be between 1 and 5"
        );

        let unknown = OffsetsParams {
            filters: Some(vec!["Luminance".to_owned(), "SII".to_owned()]),
            ..params(1)
        };
        assert!(resolve(&rig, &store, &config(), &unknown)
            .await
            .unwrap_err()
            .tool_message()
            .starts_with("filter 'SII' is not on train 'main'"));

        let outside = OffsetsParams {
            filters: Some(vec!["Ha".to_owned()]),
            reference: Some("Luminance".to_owned()),
            ..params(1)
        };
        assert_eq!(
            resolve(&rig, &store, &config(), &outside)
                .await
                .unwrap_err()
                .tool_message(),
            "reference 'Luminance' is not in filters: Ha"
        );

        let alone = OffsetsParams {
            filters: Some(vec!["Luminance".to_owned()]),
            ..params(1)
        };
        assert_eq!(
            resolve(&rig, &store, &config(), &alone)
                .await
                .unwrap_err()
                .tool_message(),
            "train 'main' has no filter to measure against 'Luminance'"
        );

        assert_eq!(bench.at(), 25_000, "nothing moved");
    }

    /// A call that names no reference takes the one the record already
    /// holds, so a second night's offsets are against the same filter
    /// as the first night's.
    #[tokio::test]
    async fn the_reference_defaults_to_the_one_the_record_holds() {
        let (store, _dir) = temp_store().await;
        let mut record = crate::store::FocusRecord::new("main", None, None, Some(filters()));
        record.set_offsets(Some("Ha"), BTreeMap::new());
        record.set_last_good(LastGood {
            filter: Some("Ha".to_owned()),
            position: 25_030,
            temperature_c: None,
            hfr: 1.0,
            at: crate::store::now_rfc3339(),
        });
        store.put(record).await.unwrap();
        let bench = Bench::new(
            25_030,
            &[("Luminance", 25_000), ("Ha", 25_030), ("OIII", 24_980)],
        );

        let view = run(&bench, &store, &params(1)).await.unwrap();

        assert_eq!(view.reference, "Ha");
        assert_eq!(view.offsets.get("Ha"), Some(&0));
        assert_eq!(view.offsets.get("Luminance"), Some(&-30));
        assert_eq!(view.offsets.get("OIII"), Some(&-50));
    }
}
