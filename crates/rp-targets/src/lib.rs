#![cfg_attr(coverage_nightly, feature(coverage_attribute))]
//! `redb`-backed store for `rp`'s imaging **plan**: the operator's target
//! list, per-sub-spec acquisition quotas, and per-target overrides for
//! grading thresholds and scheduling constraints.
//!
//! Pure storage behind one mockable trait ([`TargetStore`]) — no
//! filesystem scanning, no ephemeris, no policy. The *actuals* (how many
//! frames exist, which are good) are derived by `rp` from the filesystem
//! and per-frame sidecars; they are deliberately not in this crate. See
//! `docs/crates/rp-targets.md` for the full design, including the
//! `rp`-side integration this crate is consumed by.
#![deny(unsafe_code)]
// Curated test-scope allow list — documented in the root Cargo.toml [workspace.lints] block.
#![cfg_attr(
    test,
    allow(
        clippy::needless_pass_by_ref_mut,
        clippy::needless_pass_by_value,
        clippy::unused_async,
        clippy::unused_async_trait_impl,
        clippy::used_underscore_binding,
        clippy::significant_drop_tightening,
        clippy::significant_drop_in_scrutinee,
        clippy::cast_possible_truncation,
        clippy::cast_sign_loss,
        clippy::cast_precision_loss,
        clippy::cast_possible_wrap,
        clippy::suboptimal_flops,
        clippy::too_many_lines,
        clippy::option_if_let_else,
        clippy::match_same_arms,
        clippy::float_cmp,
        clippy::similar_names,
        clippy::struct_excessive_bools,
    )
)]

mod error;
mod migrate;
mod model;
mod redb_store;

#[cfg(any(test, feature = "mock"))]
mod memory;

pub use error::TargetStoreError;
#[cfg(any(test, feature = "mock"))]
pub use memory::InMemoryTargetStore;
pub use migrate::CURRENT_SCHEMA_VERSION;
pub use model::{
    validate_goals, AcquisitionGoal, GradingThresholds, SchedulingConstraints, Target, TargetSlug,
    TargetSlugError, WriteStamp, OPERATOR_WRITER,
};
pub use redb_store::RedbTargetStore;
// The plan value types live in `rp-vocabulary` (ADR-019); re-export the ones
// this store's public API is built from so consumers can name them as
// `rp_targets::{Binning, IcrsCoord}` without a direct vocab dependency.
pub use rp_vocabulary::{Binning, IcrsCoord};

use async_trait::async_trait;

/// The seam between the plan store and its consumer, `rp`. See
/// `docs/crates/rp-targets.md` "The `TargetStore` trait" for the full
/// contract, including upsert precedence and goal-set validation.
#[cfg_attr(any(test, feature = "mock"), mockall::automock)]
#[async_trait]
pub trait TargetStore: Send + Sync {
    /// Writes `target`, creating it or overwriting an existing row with the
    /// same slug in place — never a duplicate row. Preserves the existing
    /// row's `created_at` and `created_by` on overwrite. Validates
    /// `target.goals` per [`validate_goals`] before writing.
    async fn upsert_target(&self, target: Target) -> Result<(), TargetStoreError>;

    /// Returns the target for `slug`, including its goals, or `None` if
    /// absent.
    async fn get_target(&self, slug: &TargetSlug) -> Result<Option<Target>, TargetStoreError>;

    /// Returns every target, including goals, sorted by slug. The row
    /// count is expected to stay in the tens; callers filter/order further
    /// in Rust.
    async fn list_targets(&self) -> Result<Vec<Target>, TargetStoreError>;

    /// Removes the target for `slug`. Returns `false` if it was already
    /// absent. On-disk frames under the slug are left untouched.
    async fn delete_target(&self, slug: &TargetSlug) -> Result<bool, TargetStoreError>;

    /// Replaces `slug`'s goal set atomically, applying `stamp` to
    /// `updated_at`/`updated_by` and leaving the rest of the row untouched.
    ///
    /// # Errors
    ///
    /// Returns [`TargetStoreError::NotFound`] if `slug` has no stored
    /// target, or [`TargetStoreError::InvalidGoals`] per [`validate_goals`].
    async fn set_goals(
        &self,
        slug: &TargetSlug,
        goals: Vec<AcquisitionGoal>,
        stamp: WriteStamp,
    ) -> Result<(), TargetStoreError>;
}
