//! Planner progress: the derived snapshot the decision logic ranks
//! against, plus the one scrap of session runtime state that cannot be
//! derived from disk.
//!
//! `rp` keeps **no** frame counters. A target's actuals are the frames
//! `capture` wrote, so [`super::progress_scan`] derives them per read
//! and the result is handed to `decision::next_target` as a
//! [`PlanProgress`] snapshot. Deriving at the tool boundary rather than
//! inside the ranking keeps `next_target` a pure function of its
//! arguments (rp.md §"Dynamic Planner" → Decision Logic) and keeps its
//! tests free of the filesystem.
//!
//! [`SessionProgress`] is what remains of the old counter store: the
//! filter of the most recent `record_exposure`, feeding rp.md
//! §"Dynamic Planner" bullet 4's filter-batching tie-break. It is
//! genuinely session-scoped — "which filter is in the wheel right now"
//! is not a fact any pile of files can answer — so it stays in the
//! persisted session state (rp.md §"Session Persistence"), hence the
//! serde derives. Losing it costs at most one avoidable filter change.

use std::collections::HashMap;

use super::decision::{ExposureSpec, PlannerTarget};
use super::progress_scan::GoalProgress;

/// Normalise an optional filter name to the map key: the unfiltered
/// slot (absent, `null`, or `""` in tool parameters and target plans)
/// is the empty string.
#[must_use]
pub fn filter_key(filter: Option<&str>) -> String {
    filter.unwrap_or("").to_string()
}

/// Session runtime state the filesystem cannot supply.
#[derive(Debug, Default, serde::Serialize, serde::Deserialize)]
pub struct SessionProgress {
    /// Filter key of the most recently recorded frame, `None` before
    /// the first `record_exposure` of the session.
    last_filter_key: Option<String>,
}

impl SessionProgress {
    /// Note the filter of a completed frame. Nothing is counted — the
    /// frame itself is the count.
    pub fn record(&mut self, filter: Option<&str>) {
        self.last_filter_key = Some(filter_key(filter));
    }

    /// Filter key of the most recently recorded frame.
    #[must_use]
    pub fn last_filter_key(&self) -> Option<&str> {
        self.last_filter_key.as_deref()
    }

    /// Reset — a fresh session is a fresh night.
    pub fn clear(&mut self) {
        self.last_filter_key = None;
    }
}

/// Per-goal counts for the targets under consideration, plus the
/// session's last filter — everything `decision::next_target` needs to
/// rank candidates, gathered before the pure call.
///
/// Counts are positional: `counts[i]` belongs to `target.exposures[i]`,
/// because both project from the same `Target::goals` list in order
/// (`PlannerTarget::from` and
/// [`super::progress_scan::scan_target`] respectively). A target with
/// no entry, or an entry shorter than its plan, reads as zero rather
/// than panicking.
#[derive(Debug, Default)]
pub struct PlanProgress {
    per_target: HashMap<String, Vec<GoalProgress>>,
    last_filter_key: Option<String>,
}

impl PlanProgress {
    /// An empty snapshot carrying only the session's last filter.
    #[must_use]
    pub fn new(last_filter_key: Option<String>) -> Self {
        Self {
            per_target: HashMap::new(),
            last_filter_key,
        }
    }

    /// Record one target's derived per-goal counts, in goal order.
    pub fn insert(&mut self, target_name: &str, counts: Vec<GoalProgress>) {
        self.per_target.insert(target_name.to_string(), counts);
    }

    /// Filter key of the most recently recorded frame.
    #[must_use]
    pub fn last_filter_key(&self) -> Option<&str> {
        self.last_filter_key.as_deref()
    }

    /// This target's derived counts for plan entry `index`.
    fn counts_at(&self, target: &PlannerTarget, index: usize) -> GoalProgress {
        self.per_target
            .get(&target.name)
            .and_then(|counts| counts.get(index))
            .copied()
            .unwrap_or_default()
    }

    /// The first `exposures[]` entry whose goal the good frames on disk
    /// have not met, in plan order — the entry `get_next_target`
    /// recommends. An entry without a `count` has no finite goal and is
    /// never complete, so the walk stops there. `None` means the plan is
    /// empty or fully complete.
    ///
    /// Goals are met on `good`, not `total`: a rejected frame does not
    /// advance a plan, so a night of poor seeing keeps recommending the
    /// same entry rather than retiring it.
    #[must_use]
    pub fn next_incomplete_entry<'a>(&self, target: &'a PlannerTarget) -> Option<&'a ExposureSpec> {
        target
            .exposures
            .iter()
            .enumerate()
            .find(|(index, entry)| {
                entry
                    .count
                    .is_none_or(|goal| self.counts_at(target, *index).good < goal)
            })
            .map(|(_, entry)| entry)
    }

    /// Whether every entry of the target's plan has met its `count`.
    /// A target with no plan (or any uncounted entry) has no finite
    /// integration goal and is never exhausted.
    #[must_use]
    pub fn is_exhausted(&self, target: &PlannerTarget) -> bool {
        !target.exposures.is_empty() && self.next_incomplete_entry(target).is_none()
    }

    /// Good-to-desired fraction across the target's counted entries
    /// (clamped per entry, so an over-shot goal doesn't inflate it).
    /// A target with no counted entries reports 0.0 — it has no goal to
    /// progress toward, so bullet 3 keeps preferring it.
    #[must_use]
    pub fn fraction(&self, target: &PlannerTarget) -> f64 {
        let mut done: u32 = 0;
        let mut goal_total: u32 = 0;
        for (index, entry) in target.exposures.iter().enumerate() {
            let Some(goal) = entry.count else { continue };
            goal_total = goal_total.saturating_add(goal);
            done = done.saturating_add(self.counts_at(target, index).good.min(goal));
        }
        if goal_total == 0 {
            0.0
        } else {
            f64::from(done) / f64::from(goal_total)
        }
    }
}

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
mod tests {
    use super::*;

    fn target(name: &str, exposures: Vec<ExposureSpec>) -> PlannerTarget {
        PlannerTarget {
            name: name.to_string(),
            coord: rp_targets::IcrsCoord::try_new(0.0, 0.0).unwrap(),
            min_altitude_degrees: None,
            position_angle_degrees: None,
            exposures,
        }
    }

    fn entry(filter: Option<&str>, count: Option<u32>) -> ExposureSpec {
        ExposureSpec {
            filter: filter.map(String::from),
            duration_secs: 60.0,
            count,
        }
    }

    /// `good == total` — the common ungraded case.
    fn good(counts: &[u32]) -> Vec<GoalProgress> {
        counts
            .iter()
            .map(|&n| GoalProgress { good: n, total: n })
            .collect()
    }

    #[test]
    fn filter_key_normalises_the_unfiltered_slot() {
        assert_eq!(filter_key(None), "");
        assert_eq!(filter_key(Some("")), "");
        assert_eq!(filter_key(Some("Red")), "Red");
    }

    #[test]
    fn session_progress_remembers_only_the_last_filter() {
        let mut p = SessionProgress::default();
        assert_eq!(p.last_filter_key(), None);
        p.record(Some("Red"));
        assert_eq!(p.last_filter_key(), Some("Red"));
        p.record(None);
        assert_eq!(
            p.last_filter_key(),
            Some(""),
            "None and \"\" share the slot"
        );
        p.clear();
        assert_eq!(p.last_filter_key(), None);
    }

    #[test]
    fn a_target_with_no_derived_counts_reads_as_zero() {
        let t = target("M31", vec![entry(Some("L"), Some(2))]);
        let p = PlanProgress::default();
        assert_eq!(
            p.next_incomplete_entry(&t).unwrap().filter.as_deref(),
            Some("L")
        );
        assert!(!p.is_exhausted(&t));
        assert_eq!(p.fraction(&t), 0.0);
    }

    #[test]
    fn next_incomplete_entry_walks_the_plan_in_order() {
        let t = target(
            "M31",
            vec![entry(Some("L"), Some(2)), entry(Some("R"), Some(1))],
        );
        let mut p = PlanProgress::default();

        p.insert("M31", good(&[1, 0]));
        assert_eq!(
            p.next_incomplete_entry(&t).unwrap().filter.as_deref(),
            Some("L")
        );

        p.insert("M31", good(&[2, 0]));
        assert_eq!(
            p.next_incomplete_entry(&t).unwrap().filter.as_deref(),
            Some("R"),
            "a met goal rotates the recommendation"
        );

        p.insert("M31", good(&[2, 1]));
        assert!(p.next_incomplete_entry(&t).is_none());
        assert!(p.is_exhausted(&t));
    }

    #[test]
    fn rejected_frames_do_not_meet_a_goal() {
        let t = target("M31", vec![entry(Some("L"), Some(1))]);
        let mut p = PlanProgress::default();
        // One frame captured, graded out: total advances, good doesn't.
        p.insert("M31", vec![GoalProgress { good: 0, total: 1 }]);
        assert!(
            !p.is_exhausted(&t),
            "a rejected frame must not retire the target"
        );
        assert_eq!(
            p.next_incomplete_entry(&t).unwrap().filter.as_deref(),
            Some("L")
        );
    }

    #[test]
    fn an_uncounted_entry_pins_the_walk_and_blocks_exhaustion() {
        let t = target(
            "M31",
            vec![entry(Some("L"), Some(1)), entry(Some("R"), None)],
        );
        let mut p = PlanProgress::default();
        p.insert("M31", good(&[1, 99]));
        assert_eq!(
            p.next_incomplete_entry(&t).unwrap().filter.as_deref(),
            Some("R"),
            "an uncounted entry recommends forever"
        );
        assert!(!p.is_exhausted(&t));
    }

    #[test]
    fn duplicate_filter_entries_track_their_own_goals() {
        // Two Luminance entries with different durations are distinct
        // goals with distinct sub-specs, so the scan counts them
        // separately — the old filter-keyed counters could not.
        let mut first = entry(Some("L"), Some(2));
        first.duration_secs = 300.0;
        let mut second = entry(Some("L"), Some(2));
        second.duration_secs = 60.0;
        let t = target("M31", vec![first, second]);
        let mut p = PlanProgress::default();
        p.insert("M31", good(&[2, 1]));
        let next = p.next_incomplete_entry(&t).unwrap();
        assert_eq!(next.duration_secs, 60.0, "the first entry is complete");
        assert!(!p.is_exhausted(&t));
        p.insert("M31", good(&[2, 2]));
        assert!(p.is_exhausted(&t));
    }

    #[test]
    fn a_planless_target_is_never_exhausted() {
        let t = target("M31", Vec::new());
        let p = PlanProgress::default();
        assert!(p.next_incomplete_entry(&t).is_none());
        assert!(!p.is_exhausted(&t), "no plan means no goal to meet");
    }

    #[test]
    fn fraction_counts_good_frames_against_the_summed_goals() {
        let t = target(
            "M31",
            vec![entry(Some("L"), Some(3)), entry(Some("R"), Some(1))],
        );
        let mut p = PlanProgress::default();
        assert_eq!(p.fraction(&t), 0.0);
        p.insert("M31", good(&[1, 0]));
        assert!((p.fraction(&t) - 0.25).abs() < 1e-12);
        // Over-shooting a goal clamps: 5 L frames still count as 3 of
        // the 4-frame total.
        p.insert("M31", good(&[5, 0]));
        assert!((p.fraction(&t) - 0.75).abs() < 1e-12);
    }

    #[test]
    fn fraction_is_zero_when_the_plan_has_no_counted_entries() {
        let t = target("M31", vec![entry(Some("L"), None)]);
        let mut p = PlanProgress::default();
        p.insert("M31", good(&[7]));
        assert_eq!(p.fraction(&t), 0.0);
    }

    #[test]
    fn a_short_counts_vector_reads_as_zero_rather_than_panicking() {
        let t = target(
            "M31",
            vec![entry(Some("L"), Some(1)), entry(Some("R"), Some(1))],
        );
        let mut p = PlanProgress::default();
        p.insert("M31", good(&[1]));
        assert_eq!(
            p.next_incomplete_entry(&t).unwrap().filter.as_deref(),
            Some("R")
        );
    }
}
