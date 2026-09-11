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
use crate::store::FocusStore;
use crate::workflow::{
    focus_one, lower_middle, model_label, record_for_write, resolve_train, stale_fields, FocusRig,
    FocusTrainParams, Guiding, NoProgress, Progress, Rig, Session, TrainContext,
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
    /// Runs the record holds, the procedure's own included.
    pub runs: usize,
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
    /// Filter name to steps from the reference, which is itself 0.
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
}

impl Measured {
    /// The most recent confirmed position of one filter, which is
    /// where the procedure leaves the focuser when that filter goes
    /// back in the path.
    fn last_confirmed(&self, filter: &str) -> Option<i32> {
        self.sweeps
            .iter()
            .rev()
            .find(|sweep| sweep.confirmed && sweep.filter == filter)
            .and_then(|sweep| sweep.position)
    }

    /// Why one filter kept no offset.
    fn why_unmeasured(&self, filter: &str) -> String {
        if self.reference_placed {
            format!("no round confirmed '{filter}' and the reference together")
        } else {
            "no round's reference sweep confirmed".to_owned()
        }
    }

    /// How many sweeps did not confirm, of how many ran.
    fn shortfall(&self) -> (usize, usize) {
        let missed = self.sweeps.iter().filter(|sweep| !sweep.confirmed).count();
        (missed, self.sweeps.len())
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
    let plan = resolve(rig.active, store, params).await?;
    let started = started_state(rig.active, &plan).await?;
    let session = Session { rig, store, config };
    let (measured, fatal) = run_rounds(session, &plan, progress).await;
    let offsets = offsets_from(&measured, &plan.reference);
    // The rig goes back before anything else: the caller may be gone,
    // and the wheel is not left on the last filter the rounds swept.
    let restored = restore(rig, &plan, &started, &measured).await;
    if let Some(error) = fatal {
        return Err(error);
    }
    if offsets.len() <= 1 {
        let (missed, ran) = measured.shortfall();
        return Err(FocusModelError::Workflow(format!(
            "no filter was measured against '{}': {missed} of {ran} sweeps did not confirm",
            plan.reference
        )));
    }
    let (recorded, model) = write_offsets(store, &plan, offsets.clone()).await;
    Ok(OffsetsView {
        train_id: plan.ctx.train_id.clone(),
        reference: plan.reference.clone(),
        rounds: plan.rounds,
        unmeasured: unmeasured(&plan, &offsets, &measured),
        offsets,
        differences: measured.differences,
        sweeps: measured.sweeps,
        restored,
        recorded,
        model,
    })
}

/// Resolve the train and check every argument against it.
async fn resolve(rig: &dyn FocusRig, store: &FocusStore, params: &OffsetsParams) -> Result<Plan> {
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
    let filters = requested_filters(&ctx, params)?;
    let reference = reference_filter(&ctx, store, params, &filters).await?;
    if filters.iter().all(|name| name == &reference) {
        return Err(FocusModelError::Workflow(format!(
            "train '{}' has no filter to measure against '{reference}'",
            ctx.train_id
        )));
    }
    Ok(Plan {
        ctx,
        wheel,
        filters,
        reference,
        rounds,
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
async fn reference_filter(
    ctx: &TrainContext,
    store: &FocusStore,
    params: &OffsetsParams,
    filters: &[String],
) -> Result<String> {
    let reference = match params.reference.clone() {
        Some(name) => name,
        None => default_reference(ctx, store, filters).await?,
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
async fn default_reference(
    ctx: &TrainContext,
    store: &FocusStore,
    filters: &[String],
) -> Result<String> {
    store
        .get(&ctx.train_id)
        .await?
        .and_then(|record| record.reference_filter)
        .filter(|name| filters.iter().any(|filter| filter == name))
        .or_else(|| filters.first().cloned())
        .ok_or_else(|| {
            FocusModelError::Workflow(format!("train '{}' has no filter to measure", ctx.train_id))
        })
}

/// Read where the rig is, so the procedure can put it back there.
async fn started_state(rig: &dyn FocusRig, plan: &Plan) -> Result<Started> {
    let filter = rig.get_filter(&plan.wheel).await?;
    let position = rig
        .get_focuser_position(&plan.ctx.focuser_id)
        .await?
        .position;
    Ok(Started { filter, position })
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
                measured
                    .differences
                    .entry(filter.clone())
                    .or_default()
                    .push(position.saturating_sub(reference));
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
/// for this round and the procedure carries on; a rig that failed or a
/// caller that went away ends it, because both would meet every
/// remaining sweep the same way.
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
        }),
        Err(error @ (FocusModelError::Cancelled(_) | FocusModelError::ToolCall(_))) => Err(error),
        Err(error) => Ok(OffsetSweep {
            round,
            filter: filter.to_owned(),
            confirmed: false,
            position: None,
            hfr: None,
            error: Some(error.tool_message()),
        }),
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
async fn write_offsets(
    store: &FocusStore,
    plan: &Plan,
    offsets: BTreeMap<String, i32>,
) -> (OffsetsRecorded, String) {
    let ctx = &plan.ctx;
    let reference = plan.reference.clone();
    let written = store
        .update(&ctx.train_id, move |held| {
            let stale = stale_fields(held.as_ref(), ctx);
            let (mut record, _) = record_for_write(held, &stale, ctx);
            record.set_offsets(Some(&reference), offsets);
            Ok::<_, FocusModelError>((record, stale))
        })
        .await;
    match written {
        Ok((record, stale)) => (
            OffsetsRecorded {
                offsets_written: true,
                runs: record.run_count(None),
                error: None,
            },
            model_label(Some(&record), &stale),
        ),
        Err(error) => {
            warn!(train_id = %ctx.train_id, error = %error, "the offsets could not be recorded");
            (
                OffsetsRecorded {
                    offsets_written: false,
                    runs: 0,
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
    let filter = started.filter.clone();
    let position = filter
        .as_deref()
        .and_then(|name| measured.last_confirmed(name))
        .unwrap_or(started.position);
    let mut restored = Restored {
        filter: filter.clone(),
        position: Some(position),
        error: None,
    };
    if let Some(name) = &filter {
        if let Err(error) = rig.cleanup.set_filter(&plan.wheel, name).await {
            restored.error = Some(error.tool_message());
        }
    }
    match rig
        .cleanup
        .move_focuser(&plan.ctx.focuser_id, position)
        .await
    {
        Ok(reached) => restored.position = Some(reached),
        Err(error) => {
            let note = error.tool_message();
            restored.error = Some(match restored.error.take() {
                Some(first) => format!("{first}; {note}"),
                None => note,
            });
            restored.position = None;
        }
    }
    if let Some(error) = &restored.error {
        warn!(train_id = %plan.ctx.train_id, error, "the rig could not be put back");
    }
    restored
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
        /// Moves to answer before `move_focuser` starts failing.
        moves_before_failing: Arc<Mutex<Option<u32>>>,
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
                moves_before_failing: Arc::new(Mutex::new(None)),
            }
        }

        fn fails_moving_after(&self, moves: u32) -> &Self {
            *self.moves_before_failing.lock().unwrap() = Some(moves);
            self
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
            rig.expect_get_refocus_plan()
                .returning(|_| Box::pin(async { Ok(RefocusPlan::default()) }));
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

        /// The put-back client, which answers the same moves and reads.
        fn cleanup(&self) -> MockFocusRig {
            let mut rig = MockFocusRig::new();
            self.wire_focuser(&mut rig);
            self.wire_wheel(&mut rig);
            rig
        }

        fn wire_focuser(&self, rig: &mut MockFocusRig) {
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
            let budget = Arc::clone(&self.moves_before_failing);
            rig.expect_move_focuser().returning(move |_, to| {
                let spent = {
                    let mut budget = budget.lock().unwrap();
                    match budget.as_mut() {
                        Some(0) => true,
                        Some(left) => {
                            *left -= 1;
                            false
                        }
                        None => false,
                    }
                };
                if spent {
                    return Box::pin(async {
                        Err(FocusModelError::ToolCall(
                            "move_focuser: the focuser stopped answering".to_owned(),
                        ))
                    });
                }
                *position.lock().unwrap() = to;
                Box::pin(async move { Ok(to) })
            });
        }

        fn wire_wheel(&self, rig: &mut MockFocusRig) {
            let filter = Arc::clone(&self.filter);
            rig.expect_get_filter().returning(move |_| {
                let filter = filter.lock().unwrap().clone();
                Box::pin(async move { Ok(Some(filter)) })
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
        assert_eq!(view.recorded.runs, 6);
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

    // --- the arguments --------------------------------------------------

    #[tokio::test]
    async fn a_train_without_a_wheel_has_no_difference_to_measure() {
        let (store, _dir) = temp_store().await;
        let mut rig = MockFocusRig::new();
        rig.expect_get_train_info()
            .returning(|_| Box::pin(async { Ok(train_info(false)) }));

        let err = resolve(&rig, &store, &params(1)).await.unwrap_err();

        assert_eq!(
            err.tool_message(),
            "train 'main' has no filter wheel; an offset is a difference between filters"
        );
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
            resolve(&rig, &store, &out_of_range)
                .await
                .unwrap_err()
                .tool_message(),
            "rounds must be between 1 and 5"
        );

        let unknown = OffsetsParams {
            filters: Some(vec!["Luminance".to_owned(), "SII".to_owned()]),
            ..params(1)
        };
        assert!(resolve(&rig, &store, &unknown)
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
            resolve(&rig, &store, &outside)
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
            resolve(&rig, &store, &alone)
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
