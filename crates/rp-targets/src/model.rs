//! The plan data model: [`Target`], [`AcquisitionGoal`], and the small
//! value types they're built from. See `docs/crates/rp-targets.md` "Data
//! Model" for the authoritative field-by-field contract.

use std::time::Duration;

use rp_vocabulary::{Binning, IcrsCoord};
use serde::{Deserialize, Serialize};

use crate::error::TargetStoreError;

/// Immutable, filename-safe identity for a [`Target`].
///
/// Parse-don't-validate (see
/// `docs/skills/development-workflow.md#parse-dont-validate-for-config`):
/// a valid `TargetSlug` is always a safe directory and filename token.
/// The slug is the `{target}` token in every frame's on-disk path, so it
/// must never change once frames exist under it — renames go through
/// `Target::display_name` instead.
///
/// Two constructors, one derivation. [`TargetSlug::from_display_name`] is
/// the **only** way a human-readable name becomes a slug (whitespace runs
/// collapse to `-`, so `"NGC 7000"` → `ngc-7000`); [`TargetSlug::new`]
/// parses an already-canonical token and *rejects* whitespace rather than
/// silently normalizing it. Keeping the lossy step in exactly one place is
/// deliberate: when both constructors accepted a name and answered
/// differently, a target stored under one spelling was invisible to a
/// lookup that used the other.
#[derive(
    Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize, derive_more::Display,
)]
#[serde(transparent)]
pub struct TargetSlug(String);

impl TargetSlug {
    /// Parses an already-canonical slug token: lower-cases `input` and
    /// validates the result is non-empty and entirely `[a-z0-9-]`.
    ///
    /// Whitespace is rejected, not stripped — deriving a slug from a
    /// human-readable name is [`TargetSlug::from_display_name`]'s job, and
    /// having only one function make that choice is what keeps the stored
    /// spelling and the looked-up spelling in agreement.
    ///
    /// # Errors
    ///
    /// Returns [`TargetSlugError::Empty`] if `input` is blank,
    /// [`TargetSlugError::Whitespace`] if it contains any whitespace, or
    /// [`TargetSlugError::InvalidChars`] if a character outside
    /// `[a-z0-9-]` remains after lower-casing.
    pub fn new(input: &str) -> Result<Self, TargetSlugError> {
        if input.trim().is_empty() {
            return Err(TargetSlugError::Empty);
        }

        if input.chars().any(char::is_whitespace) {
            return Err(TargetSlugError::Whitespace {
                input: input.to_string(),
                suggestion: hyphenate(input),
            });
        }

        let normalized: String = input.chars().flat_map(char::to_lowercase).collect();

        if !normalized
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
        {
            return Err(TargetSlugError::InvalidChars {
                input: input.to_string(),
            });
        }

        Ok(Self(normalized))
    }

    /// Derives a slug from a human-readable display or catalog name:
    /// whitespace runs collapse to a single `-`, then the result is parsed
    /// by [`TargetSlug::new`]. `"NGC 7000"` → `ngc-7000`, `"Comet Test"` →
    /// `comet-test`.
    ///
    /// Idempotent — an already-valid slug contains no whitespace, so
    /// re-deriving one is a no-op. This is the single lossy name→slug step
    /// in the system; every caller that starts from a name uses it.
    ///
    /// # Errors
    ///
    /// Same as [`TargetSlug::new`], less [`TargetSlugError::Whitespace`]
    /// (which hyphenation has already resolved).
    pub fn from_display_name(display_name: &str) -> Result<Self, TargetSlugError> {
        Self::new(&hyphenate(display_name))
    }

    /// The normalized slug string, e.g. `"ngc7000-2"`.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// Words of `input`, lower-cased and joined with `-`. Shared by
/// [`TargetSlug::from_display_name`] and the [`TargetSlugError::Whitespace`]
/// suggestion so the hint always names the slug the caller would actually
/// get.
fn hyphenate(input: &str) -> String {
    input
        .split_whitespace()
        .map(|word| {
            word.chars()
                .flat_map(char::to_lowercase)
                .collect::<String>()
        })
        .collect::<Vec<_>>()
        .join("-")
}

/// Errors constructing a [`TargetSlug`]. Caller-side (rp) validation before
/// [`crate::TargetStore::upsert_target`] is ever called.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum TargetSlugError {
    /// `input` was empty, or contained only whitespace.
    #[error("target slug cannot be empty")]
    Empty,
    /// `input` contained whitespace, which a slug token never does. The
    /// error carries the derived form so a caller who passed a display
    /// name by mistake is told the slug to use instead.
    #[error("target slug {input:?} contains whitespace — use {suggestion:?}")]
    Whitespace {
        /// The original input that failed validation.
        input: String,
        /// What [`TargetSlug::from_display_name`] would derive from it.
        suggestion: String,
    },
    /// `input` contained a character outside `[a-z0-9-]` after
    /// lower-casing.
    #[error("target slug {input:?} contains characters outside [a-z0-9-]")]
    InvalidChars {
        /// The original, unnormalized input that failed validation.
        input: String,
    },
}

/// Desired frame count for one acquisition sub-spec. The
/// `(filter, binning, exposure_duration)` triple is the quota key from the
/// filename scheme — frame type is always `Light` for goals, and gain is
/// a fixed per-setup camera setting rather than a sub-spec dimension.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AcquisitionGoal {
    /// Filter name, e.g. `"Ha"`, `"L"`, `"R"`.
    pub filter: String,
    /// Frame binning ([`rp_vocabulary::Binning`], serialized as its
    /// canonical `"1x1"` string).
    pub binning: Binning,
    /// Per-frame exposure length, encoded as a humantime string on the wire.
    #[serde(with = "humantime_serde")]
    pub exposure_duration: Duration,
    /// Number of good frames desired for this sub-spec.
    pub desired_count: u32,
}

/// Per-target scheduling constraints. Each `None` field falls back to the
/// rp-config global default. Storage only — evaluated by rp's planner via
/// `rp-ephemeris`.
#[derive(Debug, Clone, Copy, Default, PartialEq, Serialize, Deserialize)]
pub struct SchedulingConstraints {
    /// Minimum altitude, in degrees, the target must be above to be
    /// eligible for imaging.
    #[serde(default)]
    pub min_altitude_degrees: Option<f64>,
    /// Minimum angular separation from the Moon, in degrees.
    #[serde(default)]
    pub min_moon_separation_degrees: Option<f64>,
    /// Maximum Moon illumination fraction (`0.0`-`1.0`) the target may be
    /// imaged under.
    #[serde(default)]
    pub max_moon_illumination_fraction: Option<f64>,
    /// Maximum `|hour angle|` from the meridian, in hours, the target may
    /// be imaged at. `None` means no meridian window constraint.
    #[serde(default)]
    pub meridian_window_hours: Option<f64>,
}

/// Per-target grading thresholds. The grading plugin owns the *meaning* of
/// these; this crate only stores the overriding values. Each `None` falls
/// back to the rp-config global default.
#[derive(Debug, Clone, Copy, Default, PartialEq, Serialize, Deserialize)]
pub struct GradingThresholds {
    /// Maximum half-flux-radius, in pixels, for a frame to grade "good".
    #[serde(default)]
    pub max_hfr_pixels: Option<f64>,
    /// Minimum detected star count for a frame to grade "good".
    #[serde(default)]
    pub min_star_count: Option<u32>,
    /// Maximum star eccentricity for a frame to grade "good".
    #[serde(default)]
    pub max_eccentricity: Option<f64>,
    /// Minimum signal-to-noise ratio for a frame to grade "good".
    #[serde(default)]
    pub min_snr: Option<f64>,
}

/// A planned pointing plus its acquisition goals — one row in the target
/// store.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Target {
    /// Immutable identity and on-disk/filename token.
    pub slug: TargetSlug,
    /// Operator-facing name; freely editable without breaking identity or
    /// existing on-disk frames.
    pub display_name: String,

    /// J2000/ICRS pointing, validated by construction ([`IcrsCoord`],
    /// ADR-019). Serialized as a nested `"coord": { "ra_hours",
    /// "dec_degrees" }` object — one canonical coordinate shape across the
    /// on-disk store and the MCP wire (no `#[serde(flatten)]`).
    pub coord: IcrsCoord,

    /// Canonical catalog name this was resolved from, e.g. `"NGC 224"`.
    /// `None` for non-catalog targets (comets, custom framings).
    #[serde(default)]
    pub catalog_ref: Option<String>,
    /// Denormalized from `rp_catalog::ResolvedTarget` at add-time.
    #[serde(default)]
    pub object_type: Option<String>,
    /// Denormalized catalog magnitude, if known.
    #[serde(default)]
    pub magnitude: Option<f64>,
    /// Denormalized catalog angular size in arcminutes, if known.
    #[serde(default)]
    pub size_arcmin: Option<f64>,

    /// Sky position angle to frame this target at, in degrees east of
    /// north (`docs/services/rp.md` § Target Store → Position angle).
    /// `None` ⇒ inherit the imaging train's configured default angle,
    /// then 0.0 north-up — resolved by rp at read time, never written
    /// back. Serde default, so pre-P2 rows deserialize as `None` (the
    /// whole migration story, exactly like the writer-identity fields;
    /// no schema-version bump).
    #[serde(default)]
    pub position_angle_degrees: Option<f64>,

    /// Scheduling priority; higher values are preferred by the planner.
    pub priority: i32,
    /// Whether the planner may select this target. Bridge-imported targets
    /// land with `active: false` pending operator review.
    pub active: bool,
    /// Desired frame counts per acquisition sub-spec.
    #[serde(default)]
    pub goals: Vec<AcquisitionGoal>,

    /// Per-target scheduling overrides. `None` uses the rp-config global
    /// default in full.
    #[serde(default)]
    pub scheduling: Option<SchedulingConstraints>,
    /// Per-target grading overrides. `None` uses the rp-config global
    /// default in full.
    #[serde(default)]
    pub grading: Option<GradingThresholds>,

    /// Free-form operator/provenance notes.
    #[serde(default)]
    pub notes: Option<String>,
    /// RFC3339 creation timestamp; set by rp at the call boundary (this
    /// crate never reads the clock). Preserved across `upsert_target` of an
    /// existing slug.
    pub created_at: String,
    /// RFC3339 last-update timestamp; set by rp at the call boundary.
    pub updated_at: String,
    /// Writer identity of the surface that created this row —
    /// [`OPERATOR_WRITER`] for the operator MCP tools, an import's
    /// `source.kind` (e.g. `"planetarium-bridge"`) otherwise. Preserved
    /// across `upsert_target` of an existing slug, like `created_at`.
    /// Rows written before this field existed deserialize as
    /// operator-created.
    #[serde(default = "default_writer")]
    pub created_by: String,
    /// Writer identity of the last write, same domain as `created_by`.
    /// `!active && updated_by == "<import kind>"` is the "pending and
    /// unedited since import" predicate rp's import dedup keys on
    /// (`docs/services/rp.md` § Target Store).
    #[serde(default = "default_writer")]
    pub updated_by: String,
}

/// The writer-identity value every operator-surface write stamps.
pub const OPERATOR_WRITER: &str = "operator";

fn default_writer() -> String {
    OPERATOR_WRITER.to_string()
}

/// Last-write attribution for [`crate::TargetStore::set_goals`]: the store
/// never reads the clock, so rp supplies both stamps at the call boundary,
/// exactly as it does for the [`Target`] fields on `upsert_target`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WriteStamp {
    /// RFC3339 timestamp for `Target::updated_at`.
    pub updated_at: String,
    /// Writer identity for `Target::updated_by`.
    pub updated_by: String,
}

/// Validates a goal set for [`crate::TargetStore::upsert_target`] and
/// [`crate::TargetStore::set_goals`]: no two goals may share the same
/// `(filter, binning, exposure_duration)` key, and no goal may have a zero
/// `desired_count` or zero `exposure_duration`.
///
/// # Errors
///
/// Returns [`TargetStoreError::InvalidGoals`] naming the offending goal.
pub fn validate_goals(goals: &[AcquisitionGoal]) -> Result<(), TargetStoreError> {
    for goal in goals {
        if goal.desired_count == 0 {
            return Err(TargetStoreError::InvalidGoals {
                reason: format!(
                    "goal for filter {:?} at {} has desired_count == 0",
                    goal.filter, goal.binning
                ),
            });
        }
        if goal.exposure_duration.is_zero() {
            return Err(TargetStoreError::InvalidGoals {
                reason: format!(
                    "goal for filter {:?} at {} has a zero exposure_duration",
                    goal.filter, goal.binning
                ),
            });
        }
    }

    for (i, a) in goals.iter().enumerate() {
        // `i < len`, so `i + 1` is a valid (possibly empty) tail start.
        for b in goals.get(i.saturating_add(1)..).unwrap_or_default() {
            if a.filter == b.filter
                && a.binning == b.binning
                && a.exposure_duration == b.exposure_duration
            {
                return Err(TargetStoreError::InvalidGoals {
                    reason: format!(
                        "duplicate goal key: filter {:?}, binning {}, exposure_duration {:?}",
                        a.filter, a.binning, a.exposure_duration
                    ),
                });
            }
        }
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn slug_lowercases() {
        let slug = TargetSlug::new("NGC7000").unwrap();
        assert_eq!(slug.as_str(), "ngc7000");
    }

    #[test]
    fn slug_allows_hyphens_and_digits() {
        let slug = TargetSlug::new("ngc7000-2").unwrap();
        assert_eq!(slug.as_str(), "ngc7000-2");
    }

    #[test]
    fn slug_rejects_empty_input() {
        let err = TargetSlug::new("   ").unwrap_err();
        assert_eq!(err, TargetSlugError::Empty);
    }

    #[test]
    fn slug_rejects_whitespace_and_suggests_the_derived_form() {
        let err = TargetSlug::new("NGC 7000").unwrap_err();
        assert_eq!(
            err,
            TargetSlugError::Whitespace {
                input: "NGC 7000".to_string(),
                suggestion: "ngc-7000".to_string(),
            }
        );
    }

    #[test]
    fn display_name_derivation_hyphenates_whitespace_runs() {
        assert_eq!(
            TargetSlug::from_display_name("NGC 7000").unwrap().as_str(),
            "ngc-7000"
        );
        assert_eq!(
            TargetSlug::from_display_name("  Comet   Test  ")
                .unwrap()
                .as_str(),
            "comet-test"
        );
    }

    /// A doubled space is a typo, not a different target: a run of
    /// whitespace collapses to one hyphen, so the accidental spelling
    /// lands on the same slug — and therefore the same stored row and
    /// the same on-disk directory — as the intended one.
    #[test]
    fn a_doubled_space_derives_the_same_slug_as_a_single_one() {
        let intended = TargetSlug::from_display_name("M 31").unwrap();
        assert_eq!(TargetSlug::from_display_name("M  31").unwrap(), intended);
        assert_eq!(TargetSlug::from_display_name(" M 31 ").unwrap(), intended);
        assert_eq!(TargetSlug::from_display_name("M\t31").unwrap(), intended);
        assert_eq!(intended.as_str(), "m-31");
    }

    /// The property that makes one derivation safe: re-deriving a slug
    /// that already exists cannot move it, so a stored row stays
    /// reachable no matter how many times a name round-trips.
    #[test]
    fn display_name_derivation_is_idempotent() {
        let once = TargetSlug::from_display_name("M 31").unwrap();
        let twice = TargetSlug::from_display_name(once.as_str()).unwrap();
        assert_eq!(once, twice);
        assert_eq!(once.as_str(), "m-31");
    }

    /// The bug this shape exists to prevent: the derivation and the
    /// parser must never disagree about what a name means. `new` refuses
    /// to guess instead of answering differently.
    #[test]
    fn the_parser_never_answers_a_name_differently_than_the_derivation() {
        let derived = TargetSlug::from_display_name("M 31").unwrap();
        assert!(TargetSlug::new("M 31").is_err());
        assert_eq!(TargetSlug::new(derived.as_str()).unwrap(), derived);
    }

    #[test]
    fn slug_rejects_out_of_charset_characters() {
        let err = TargetSlug::new("m33!").unwrap_err();
        assert_eq!(
            err,
            TargetSlugError::InvalidChars {
                input: "m33!".to_string()
            }
        );
    }

    // `Binning`'s Display/FromStr round-trip is tested in `rp-vocabulary`,
    // where the type now lives (ADR-019).

    fn goal(filter: &str, x: u8, y: u8, secs: u64, desired_count: u32) -> AcquisitionGoal {
        AcquisitionGoal {
            filter: filter.to_string(),
            binning: Binning { x, y },
            exposure_duration: Duration::from_secs(secs),
            desired_count,
        }
    }

    #[test]
    fn validate_goals_accepts_distinct_keys() {
        let goals = vec![goal("Ha", 1, 1, 300, 20), goal("Ha", 1, 1, 120, 20)];
        validate_goals(&goals).unwrap();
    }

    #[test]
    fn validate_goals_rejects_duplicate_key() {
        let goals = vec![goal("Ha", 1, 1, 300, 20), goal("Ha", 1, 1, 300, 10)];
        let err = validate_goals(&goals).unwrap_err();
        assert!(matches!(err, TargetStoreError::InvalidGoals { .. }));
    }

    #[test]
    fn validate_goals_rejects_zero_desired_count() {
        let goals = vec![goal("Ha", 1, 1, 300, 0)];
        let err = validate_goals(&goals).unwrap_err();
        assert!(matches!(err, TargetStoreError::InvalidGoals { .. }));
    }

    #[test]
    fn validate_goals_rejects_zero_exposure_duration() {
        let goals = vec![goal("Ha", 1, 1, 0, 20)];
        let err = validate_goals(&goals).unwrap_err();
        assert!(matches!(err, TargetStoreError::InvalidGoals { .. }));
    }

    // A row serialized before the writer-identity fields existed must
    // deserialize as operator-owned — this is the whole redb migration
    // story for the additive change (no schema-version step).
    #[test]
    fn pre_writer_identity_row_deserializes_as_operator_owned() {
        let old_row = serde_json::json!({
            "slug": "m31",
            "display_name": "M 31",
            "coord": { "ra_hours": 0.7123, "dec_degrees": 41.2688 },
            "priority": 0,
            "active": true,
            "created_at": "2026-07-22T00:00:00Z",
            "updated_at": "2026-07-22T00:00:00Z"
        });
        let target: Target = serde_json::from_value(old_row).unwrap();
        assert_eq!(target.created_by, OPERATOR_WRITER);
        assert_eq!(target.updated_by, OPERATOR_WRITER);
    }

    // Same migration story for the P2 framing field: a pre-P2 row has
    // no `position_angle_degrees` and must deserialize as `None`
    // (inherit the train default) — no schema-version step.
    #[test]
    fn pre_position_angle_row_deserializes_as_inheriting() {
        let old_row = serde_json::json!({
            "slug": "m31",
            "display_name": "M 31",
            "coord": { "ra_hours": 0.7123, "dec_degrees": 41.2688 },
            "priority": 0,
            "active": true,
            "created_at": "2026-07-22T00:00:00Z",
            "updated_at": "2026-07-22T00:00:00Z"
        });
        let target: Target = serde_json::from_value(old_row).unwrap();
        assert_eq!(target.position_angle_degrees, None);
    }
}
