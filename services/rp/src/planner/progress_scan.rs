//! On-disk progress derivation (rp.md § Target Store → Progress
//! derivation).
//!
//! `rp` stores no progress counters. A target's actuals are the frames
//! `capture` already wrote, so every read walks the night directories
//! under the target's slug, parses each filename back through the
//! configured naming templates, and buckets frames by the
//! `(filter, binning, exposure_duration)` triple — which is exactly an
//! [`AcquisitionGoal`]'s identity. Because the same two compiled
//! templates drive `capture`'s writes and this module's reads, the scan
//! finds whatever layout the operator configured rather than a hard-coded
//! one.
//!
//! Good-vs-rejected is decided against the target's *effective* grading
//! thresholds (its own overrides, field-wise over
//! `target_store.default_grading`), read from each frame's sidecar
//! `grading` section. A frame is rejected only on evidence: no sidecar,
//! no section, or no value for the judged metric all count as good.
//! When no threshold is effective there is nothing a sidecar could
//! contradict, so the sidecar reads are skipped entirely and
//! `good == total`.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::time::Duration;

use rp_targets::{AcquisitionGoal, Binning, GradingThresholds, Target};
use rp_vocabulary::FrameType;
use tracing::debug;

use crate::config::naming_template::{NamingTemplates, TemplateFields};

/// The literal `{filter}` value `capture` renders for a train with no
/// filter wheel (rp.md § Capture Tool Details). The scan maps it back to
/// the unfiltered goal slot so an OSC rig's plan can progress at all.
const UNFILTERED_TOKEN: &str = "NA";

/// One goal's derived counts. `total` is every frame captured against
/// the goal's sub-spec; `good` is the subset not demonstrably rejected.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct GoalProgress {
    pub good: u32,
    pub total: u32,
}

/// A bucket key: the `(filter, binning, exposure_duration)` triple that
/// identifies an [`AcquisitionGoal`]. The filter is normalized — the
/// unfiltered slot is the empty string, whether it arrived as `""` from
/// a goal or as `"NA"` from a filename.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct SubSpec {
    filter: String,
    binning: Binning,
    exposure_duration: Duration,
}

impl SubSpec {
    fn from_goal(goal: &AcquisitionGoal) -> Self {
        Self {
            filter: normalize_filter(&goal.filter),
            binning: goal.binning,
            exposure_duration: goal.exposure_duration,
        }
    }

    /// The sub-spec a parsed frame belongs to, or `None` when the
    /// filename didn't yield all three parts. Every one of them is a
    /// required token of `file_naming_pattern`, so `None` here means the
    /// filename didn't match the pattern in the first place.
    fn from_fields(fields: &TemplateFields) -> Option<Self> {
        Some(Self {
            filter: normalize_filter(fields.filter.as_deref()?),
            binning: fields.binning?,
            exposure_duration: fields.exposure_duration?,
        })
    }
}

fn normalize_filter(filter: &str) -> String {
    if filter == UNFILTERED_TOKEN {
        String::new()
    } else {
        filter.to_string()
    }
}

/// Whether `thresholds` judges anything at all. All-`None` means no
/// sidecar can contradict it, which lets the scan skip every file open.
const fn judges_anything(thresholds: &GradingThresholds) -> bool {
    thresholds.max_hfr_pixels.is_some()
        || thresholds.min_star_count.is_some()
        || thresholds.max_eccentricity.is_some()
        || thresholds.min_snr.is_some()
}

/// A target's own grading overrides applied field-wise over the
/// configured default (rp.md § Progress derivation). `update_target`
/// replaces the whole override object, so a present `Target::grading`
/// still inherits per-field from the default for the fields it leaves
/// `None`.
#[must_use]
pub fn effective_thresholds(
    target: &Target,
    default: Option<GradingThresholds>,
) -> GradingThresholds {
    let default = default.unwrap_or_default();
    let Some(own) = target.grading else {
        return default;
    };
    GradingThresholds {
        max_hfr_pixels: own.max_hfr_pixels.or(default.max_hfr_pixels),
        min_star_count: own.min_star_count.or(default.min_star_count),
        max_eccentricity: own.max_eccentricity.or(default.max_eccentricity),
        min_snr: own.min_snr.or(default.min_snr),
    }
}

/// The grading metrics `rp` reads from a frame's sidecar
/// `sections.grading`. Every field is optional and unknown keys are
/// ignored: the section belongs to the grading plugin, and `rp` only
/// picks out the four metrics its thresholds judge.
#[derive(Debug, Default, serde::Deserialize)]
struct GradingMetrics {
    #[serde(default)]
    hfr: Option<f64>,
    #[serde(default)]
    star_count: Option<u32>,
    #[serde(default)]
    eccentricity: Option<f64>,
    #[serde(default)]
    snr: Option<f64>,
}

impl GradingMetrics {
    /// Whether any present metric violates its threshold. Absent metrics
    /// never reject — a frame is rejected only on evidence.
    fn violates(&self, thresholds: &GradingThresholds) -> bool {
        let over = |value: Option<f64>, limit: Option<f64>| matches!((value, limit), (Some(v), Some(l)) if v > l);
        let under = |value: Option<f64>, limit: Option<f64>| matches!((value, limit), (Some(v), Some(l)) if v < l);
        over(self.hfr, thresholds.max_hfr_pixels)
            || over(self.eccentricity, thresholds.max_eccentricity)
            || under(self.snr, thresholds.min_snr)
            || matches!(
                (self.star_count, thresholds.min_star_count),
                (Some(count), Some(min)) if count < min
            )
    }
}

/// Derive per-goal progress for one target, in goal order.
///
/// Returns all-zero counts (one per goal) when no naming templates are
/// configured: without `session.file_naming_pattern` `capture` writes
/// flat `<doc_uuid_8>.fits` files that carry no target, so there is
/// nothing on disk to attribute. Filesystem errors are logged and
/// treated as "no frames here" rather than propagated — a progress read
/// must never be the thing that ends a night.
pub async fn scan_target(
    data_directory: &Path,
    templates: Option<&NamingTemplates>,
    target: &Target,
    thresholds: GradingThresholds,
) -> Vec<GoalProgress> {
    let empty = || vec![GoalProgress::default(); target.goals.len()];
    let Some(templates) = templates else {
        return empty();
    };
    if target.goals.is_empty() {
        return Vec::new();
    }

    let scan = Scan {
        templates,
        slug: target.slug.as_str(),
        // Nothing to contradict an empty threshold set, so skip every
        // sidecar read and let each frame count good.
        grade: judges_anything(&thresholds),
        thresholds,
    };
    let mut buckets: HashMap<SubSpec, GoalProgress> = HashMap::new();
    for dir in candidate_directories(data_directory, templates, target.slug.as_str()).await {
        scan.collect_frames(&dir.path, &dir.fields, &mut buckets)
            .await;
    }

    allocate(&target.goals, &buckets)
}

/// One target's scan parameters, fixed for the whole walk.
struct Scan<'a> {
    templates: &'a NamingTemplates,
    slug: &'a str,
    /// Whether any threshold is set — `false` skips sidecar reads.
    grade: bool,
    thresholds: GradingThresholds,
}

/// A directory the walk kept, paired with whatever the directory
/// template could tell us about it (frame type, night date, target).
struct CandidateDir {
    path: PathBuf,
    fields: TemplateFields,
}

/// Walk `data_directory` to the depth `directory_pattern` renders to,
/// keeping directories whose parsed `{target}` is this slug (or that
/// carry no target token at all — the per-file check is authoritative,
/// since `{target}` is a *required* token of `file_naming_pattern`).
async fn candidate_directories(
    data_directory: &Path,
    templates: &NamingTemplates,
    slug: &str,
) -> Vec<CandidateDir> {
    let depth = templates.directory.path_component_count();
    // Relative components accumulated so far, joined with `/` for the
    // template's own separator regardless of the host OS.
    let mut frontier: Vec<(PathBuf, Vec<String>)> =
        vec![(data_directory.to_path_buf(), Vec::new())];
    for _ in 0..depth {
        let mut next = Vec::new();
        for (path, components) in frontier {
            let mut entries = match tokio::fs::read_dir(&path).await {
                Ok(entries) => entries,
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => continue,
                Err(e) => {
                    debug!(path = %path.display(), error = %e, "progress scan: unreadable directory");
                    continue;
                }
            };
            loop {
                match entries.next_entry().await {
                    Ok(Some(entry)) => {
                        let Ok(file_type) = entry.file_type().await else {
                            continue;
                        };
                        if !file_type.is_dir() {
                            continue;
                        }
                        let Some(name) = entry.file_name().to_str().map(String::from) else {
                            continue;
                        };
                        let mut components = components.clone();
                        components.push(name);
                        next.push((entry.path(), components));
                    }
                    Ok(None) => break,
                    Err(e) => {
                        debug!(path = %path.display(), error = %e, "progress scan: directory read failed");
                        break;
                    }
                }
            }
        }
        frontier = next;
    }

    frontier
        .into_iter()
        .filter_map(|(path, components)| {
            let relative = components.join("/");
            let Some(fields) = templates.directory.parse(&relative) else {
                debug!(path = %path.display(), "progress scan: directory does not match directory_pattern");
                return None;
            };
            // A directory whose target token names someone else is a
            // different target's frames — skip without opening it.
            if fields
                .target
                .as_ref()
                .is_some_and(|parsed| parsed.as_str() != slug)
            {
                return None;
            }
            Some(CandidateDir { path, fields })
        })
        .collect()
}

impl Scan<'_> {
    /// Count every `.fits` frame in one candidate directory into `buckets`.
    async fn collect_frames(
        &self,
        dir: &Path,
        dir_fields: &TemplateFields,
        buckets: &mut HashMap<SubSpec, GoalProgress>,
    ) {
        let mut entries = match tokio::fs::read_dir(dir).await {
            Ok(entries) => entries,
            Err(e) => {
                debug!(path = %dir.display(), error = %e, "progress scan: unreadable frame directory");
                return;
            }
        };
        loop {
            let entry = match entries.next_entry().await {
                Ok(Some(entry)) => entry,
                Ok(None) => break,
                Err(e) => {
                    debug!(path = %dir.display(), error = %e, "progress scan: frame read failed");
                    break;
                }
            };
            let path = entry.path();
            // Sidecars share a stem with their FITS file; counting both
            // would double-count every frame.
            if path.extension().and_then(|e| e.to_str()) != Some("fits") {
                continue;
            }
            let Some(stem) = path.file_stem().and_then(|s| s.to_str()) else {
                continue;
            };
            let Some(fields) = self.templates.file.parse(stem) else {
                debug!(path = %path.display(), "progress scan: filename does not match file_naming_pattern");
                continue;
            };

            // The filename is authoritative where it speaks; the directory
            // fills in what the filename pattern omits (typically the frame
            // type and night date).
            let frame_type = fields.frame_type.or(dir_fields.frame_type);
            if frame_type.is_some_and(|ft| ft != FrameType::Light) {
                continue;
            }
            let frame_target = fields.target.as_ref().or(dir_fields.target.as_ref());
            if frame_target.is_none_or(|parsed| parsed.as_str() != self.slug) {
                continue;
            }
            let Some(spec) = SubSpec::from_fields(&fields) else {
                continue;
            };

            let counts = buckets.entry(spec).or_default();
            counts.total = counts.total.saturating_add(1);
            if !self.grade || !is_rejected(&path, &self.thresholds).await {
                counts.good = counts.good.saturating_add(1);
            }
        }
    }
}

/// Read a frame's sidecar `grading` section and judge it. Anything that
/// isn't a readable section carrying a violating metric counts as good.
async fn is_rejected(frame: &Path, thresholds: &GradingThresholds) -> bool {
    let sidecar = frame.with_extension("json");
    let Ok(bytes) = tokio::fs::read(&sidecar).await else {
        return false;
    };
    let Ok(document) = serde_json::from_slice::<serde_json::Value>(&bytes) else {
        debug!(path = %sidecar.display(), "progress scan: sidecar is not valid JSON");
        return false;
    };
    let Some(section) = document.pointer("/sections/grading") else {
        return false;
    };
    match serde_json::from_value::<GradingMetrics>(section.clone()) {
        Ok(metrics) => metrics.violates(thresholds),
        Err(e) => {
            debug!(path = %sidecar.display(), error = %e, "progress scan: unreadable grading section");
            false
        }
    }
}

/// Pair each goal with the bucket its sub-spec keys.
///
/// One-to-one by construction: `rp-targets`' `validate_goals` rejects a
/// goal set that repeats a `(filter, binning, exposure_duration)`
/// triple, so no two goals can key the same bucket and no frame is ever
/// claimed twice. Counts are reported as found — a goal that has been
/// over-shot says so rather than clamping at `desired_count`, and the
/// planner does its own clamping when it turns counts into a completion
/// fraction.
fn allocate(
    goals: &[AcquisitionGoal],
    buckets: &HashMap<SubSpec, GoalProgress>,
) -> Vec<GoalProgress> {
    goals
        .iter()
        .map(|goal| {
            buckets
                .get(&SubSpec::from_goal(goal))
                .copied()
                .unwrap_or_default()
        })
        .collect()
}

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
mod tests {
    use super::*;

    fn thresholds(max_hfr: Option<f64>, min_stars: Option<u32>) -> GradingThresholds {
        GradingThresholds {
            max_hfr_pixels: max_hfr,
            min_star_count: min_stars,
            ..Default::default()
        }
    }

    #[test]
    fn all_none_thresholds_judge_nothing() {
        assert!(!judges_anything(&GradingThresholds::default()));
        assert!(judges_anything(&thresholds(Some(3.0), None)));
        assert!(judges_anything(&thresholds(None, Some(20))));
    }

    #[test]
    fn na_normalizes_to_the_unfiltered_slot() {
        assert_eq!(normalize_filter("NA"), "");
        assert_eq!(normalize_filter(""), "");
        assert_eq!(normalize_filter("Ha"), "Ha");
        // Case matters: only the exact literal `capture` writes maps.
        assert_eq!(normalize_filter("na"), "na");
    }

    #[test]
    fn an_absent_metric_never_rejects() {
        let limits = thresholds(Some(3.0), Some(100));
        assert!(!GradingMetrics::default().violates(&limits));
        assert!(!GradingMetrics {
            star_count: Some(900),
            ..Default::default()
        }
        .violates(&limits));
    }

    #[test]
    fn any_one_violated_metric_rejects() {
        let limits = thresholds(Some(3.0), Some(100));
        assert!(GradingMetrics {
            hfr: Some(9.9),
            star_count: Some(900),
            ..Default::default()
        }
        .violates(&limits));
        assert!(GradingMetrics {
            hfr: Some(2.1),
            star_count: Some(12),
            ..Default::default()
        }
        .violates(&limits));
        assert!(!GradingMetrics {
            hfr: Some(2.1),
            star_count: Some(900),
            ..Default::default()
        }
        .violates(&limits));
    }

    #[test]
    fn a_metric_exactly_at_its_threshold_is_good() {
        // The contract is `>` / `<`, not `>=` / `<=`: a frame sitting
        // exactly on the limit passes.
        let limits = thresholds(Some(3.0), Some(100));
        assert!(!GradingMetrics {
            hfr: Some(3.0),
            star_count: Some(100),
            ..Default::default()
        }
        .violates(&limits));
    }

    #[test]
    fn per_target_overrides_fall_back_field_wise_to_the_default() {
        let default = GradingThresholds {
            max_hfr_pixels: Some(3.0),
            min_star_count: Some(20),
            ..Default::default()
        };
        let mut target = sample_target();
        target.grading = Some(GradingThresholds {
            max_hfr_pixels: Some(10.0),
            ..Default::default()
        });
        let effective = effective_thresholds(&target, Some(default));
        assert_eq!(effective.max_hfr_pixels, Some(10.0));
        assert_eq!(
            effective.min_star_count,
            Some(20),
            "a field the override leaves None still inherits the default"
        );
    }

    #[test]
    fn no_override_uses_the_default_whole() {
        let default = thresholds(Some(3.0), Some(20));
        let target = sample_target();
        assert_eq!(effective_thresholds(&target, Some(default)), default);
        assert_eq!(
            effective_thresholds(&target, None),
            GradingThresholds::default()
        );
    }

    #[test]
    fn a_goal_with_no_matching_frames_reports_zero() {
        let goals = vec![goal("L", 4), goal("R", 4)];
        let mut buckets = HashMap::new();
        buckets.insert(
            SubSpec::from_goal(&goals[0]),
            GoalProgress { good: 1, total: 1 },
        );
        let progress = allocate(&goals, &buckets);
        assert_eq!(progress[0], GoalProgress { good: 1, total: 1 });
        assert_eq!(progress[1], GoalProgress::default());
    }

    #[test]
    fn an_over_shot_goal_reports_the_true_count() {
        // `validate_goals` guarantees one goal per sub-spec, so there is
        // nobody to hand the surplus to — and hiding it would under-report
        // what is actually on disk.
        let goals = vec![goal("L", 2)];
        let mut buckets = HashMap::new();
        buckets.insert(
            SubSpec::from_goal(&goals[0]),
            GoalProgress { good: 5, total: 5 },
        );
        let progress = allocate(&goals, &buckets);
        assert_eq!(progress[0], GoalProgress { good: 5, total: 5 });
    }

    #[test]
    fn rejected_frames_keep_total_above_good() {
        let goals = vec![goal("L", 4)];
        let mut buckets = HashMap::new();
        buckets.insert(
            SubSpec::from_goal(&goals[0]),
            GoalProgress { good: 1, total: 3 },
        );
        let progress = allocate(&goals, &buckets);
        assert_eq!(progress[0], GoalProgress { good: 1, total: 3 });
    }

    fn goal(filter: &str, desired_count: u32) -> AcquisitionGoal {
        AcquisitionGoal {
            filter: filter.to_string(),
            binning: Binning { x: 1, y: 1 },
            exposure_duration: Duration::from_mins(5),
            desired_count,
        }
    }

    fn sample_target() -> Target {
        Target {
            slug: rp_targets::TargetSlug::new("m31").unwrap(),
            display_name: "M31".to_string(),
            catalog_ref: None,
            object_type: None,
            magnitude: None,
            size_arcmin: None,
            coord: rp_targets::IcrsCoord::try_new(0.712, 41.269).unwrap(),
            active: true,
            priority: 0,
            position_angle_degrees: None,
            goals: vec![goal("L", 4)],
            scheduling: None,
            grading: None,
            notes: None,
            created_at: "2026-07-31T00:00:00Z".to_string(),
            updated_at: "2026-07-31T00:00:00Z".to_string(),
            created_by: "operator".to_string(),
            updated_by: "operator".to_string(),
        }
    }
}
