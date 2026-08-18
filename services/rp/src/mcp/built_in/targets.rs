//! Target store MCP tools (`docs/services/rp.md` § Target Store):
//! `add_target` / `get_target` / `list_targets` / `update_target` /
//! `delete_target` / `set_goals`, backed by [`rp_targets::TargetStore`].
//!
//! The store is the sole source of planner targets ([`super::planner`]'s
//! `get_next_target` / `record_exposure` / `get_session_progress` read
//! its active rows); its settings live under the `target_store` config
//! key (`crate::config::target_store`). Progress is derived per read
//! from the frames on disk by [`crate::planner::progress_scan`] — never
//! stored — so `good`/`total` here are the same numbers the planner
//! decides on (rp.md § Progress derivation).

use rmcp::handler::server::wrapper::Parameters;
use rmcp::model::{CallToolResult, ContentBlock};
use rmcp::{tool, tool_router};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use rp_targets::{
    AcquisitionGoal, IcrsCoord, Target, TargetSlug, TargetStore, WriteStamp, OPERATOR_WRITER,
};

use crate::config::target_store::GradingWire;
use crate::equipment::EquipmentRegistry;
use crate::planner::goal_wire::GoalWire;

use super::super::handler::McpHandler;
use super::super::{tool_error, tool_success};

/// Coordinates are the same object when within Decision 3's dedup
/// tolerance (~10 arcmin, `docs/plans/planetarium-target-import.md`).
/// A flat, `cos(dec)`-weighted approximation rather than a full
/// great-circle formula — at this scale the two are indistinguishable,
/// and every case this guards against (a genuine re-add vs. a framing
/// a degree away) is nowhere near the boundary.
const DEDUP_TOLERANCE_DEGREES: f64 = 10.0 / 60.0;

#[derive(Debug, Deserialize, JsonSchema)]
pub struct AddTargetParams {
    /// Catalog name, resolved via `resolve_target`. Mutually exclusive
    /// with `display_name`; not accepted with `source`.
    #[serde(default)]
    pub catalog_ref: Option<String>,
    /// Custom target name. Requires `ra_hours` + `dec_degrees`.
    /// Mutually exclusive with `catalog_ref`; not accepted with
    /// `source` (rp names imports itself).
    #[serde(default)]
    pub display_name: Option<String>,
    /// Required with `display_name` and with `source`; optional with
    /// `catalog_ref` to override the catalog centroid with a
    /// precisely-framed pointing.
    #[serde(default)]
    pub ra_hours: Option<f64>,
    #[serde(default)]
    pub dec_degrees: Option<f64>,
    /// Defaults to true. Not accepted with `source` — imports always
    /// land paused pending operator review.
    #[serde(default)]
    pub active: Option<bool>,
    /// Defaults to `targets.default_goals` from config when omitted.
    #[serde(default)]
    pub goals: Option<Vec<GoalWire>>,
    /// Per-target scheduling overrides (Decision 9 — altitude-gating
    /// parity, `docs/plans/planetarium-target-import.md`). Omitted
    /// fields fall back to `targets.default_scheduling` from config.
    #[serde(default)]
    pub scheduling: Option<SchedulingWire>,
    /// Not accepted with `source` — rp writes the provenance line into
    /// `notes` itself for imports.
    #[serde(default)]
    pub notes: Option<String>,
    /// Framing angle in degrees east of north, `0.0 ≤ angle < 360.0`
    /// (rp.md § Target Store → Position angle). Omitted → inherit the
    /// imaging train's configured default at read time. Not accepted
    /// with `source` — imports never carry an angle.
    #[serde(default)]
    pub position_angle_degrees: Option<f64>,
    /// Per-target grading overrides (rp.md § Progress derivation).
    /// Omitted fields fall back to `target_store.default_grading` from
    /// config. Not accepted with `source` — imports carry no thresholds.
    #[serde(default)]
    pub grading: Option<GradingWire>,
    /// Selects the import form (rp.md § Target Store → Import form):
    /// bare `ra_hours` + `dec_degrees` plus this typed provenance.
    #[serde(default)]
    pub source: Option<SourceWire>,
}

/// `add_target`'s `source` parameter — typed provenance from a
/// non-operator writer (the planetarium bridge today). `kind` becomes
/// the row's `created_by`/`updated_by` writer identity; all three
/// fields feed the human-readable provenance line in `notes`.
#[derive(Debug, Deserialize, JsonSchema)]
pub struct SourceWire {
    /// Writer identity, e.g. `"planetarium-bridge"`. `"operator"` is
    /// reserved for the operator surface and rejected here.
    pub kind: String,
    /// Originating client address, `"ip:port"`.
    pub client: String,
    /// RFC3339 receipt time at the importing service.
    pub received_at: String,
}

/// The wire shape of `add_target`/`update_target`'s `scheduling`
/// parameter — field-for-field [`rp_targets::SchedulingConstraints`],
/// restated here (rather than deriving `JsonSchema` on the crate type
/// directly) because `rp-targets` carries no `schemars` dependency, the
/// same reasoning [`GoalWire`] follows for goals.
#[derive(Debug, Clone, Copy, Default, Deserialize, JsonSchema)]
pub struct SchedulingWire {
    #[serde(default)]
    pub min_altitude_degrees: Option<f64>,
    #[serde(default)]
    pub min_moon_separation_degrees: Option<f64>,
    #[serde(default)]
    pub max_moon_illumination_fraction: Option<f64>,
    #[serde(default)]
    pub meridian_window_hours: Option<f64>,
}

impl From<SchedulingWire> for rp_targets::SchedulingConstraints {
    fn from(w: SchedulingWire) -> Self {
        Self {
            min_altitude_degrees: w.min_altitude_degrees,
            min_moon_separation_degrees: w.min_moon_separation_degrees,
            max_moon_illumination_fraction: w.max_moon_illumination_fraction,
            meridian_window_hours: w.meridian_window_hours,
        }
    }
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct GetTargetParams {
    pub slug: String,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct ListTargetsParams {
    #[serde(default)]
    pub active_only: Option<bool>,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct UpdateTargetParams {
    pub slug: String,
    #[serde(default)]
    pub display_name: Option<String>,
    #[serde(default)]
    pub ra_hours: Option<f64>,
    #[serde(default)]
    pub dec_degrees: Option<f64>,
    #[serde(default)]
    pub active: Option<bool>,
    #[serde(default)]
    pub priority: Option<i32>,
    /// Replaces the target's scheduling overrides wholesale when
    /// present (not a field-wise merge — omit the whole parameter to
    /// leave the existing overrides untouched).
    #[serde(default)]
    pub scheduling: Option<SchedulingWire>,
    #[serde(default)]
    pub notes: Option<String>,
    /// Framing angle in degrees east of north, `0.0 ≤ angle < 360.0`.
    /// The one field with explicit-null semantics (rp.md § Target
    /// Store → Position angle): omitted → untouched; a number → set;
    /// an explicit `null` → cleared back to inherit-the-train-default
    /// — "blank" and "0.0 north-up" must stay distinguishable for the
    /// P4 inbox.
    #[serde(default, deserialize_with = "deserialize_patch")]
    #[schemars(with = "Option<f64>")]
    pub position_angle_degrees: Patch<f64>,
    /// Replaces the target's grading overrides wholesale when present,
    /// like `scheduling`; an explicit `null` clears them back to
    /// inherit-`target_store.default_grading` (rp.md § Progress
    /// derivation).
    #[serde(default, deserialize_with = "deserialize_patch")]
    #[schemars(with = "Option<GradingWire>")]
    pub grading: Patch<GradingWire>,
}

/// Tri-state PATCH field: distinguishes an absent key (leave the
/// stored value untouched) from an explicit JSON `null` (clear back to
/// inherit) and a present value (set).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum Patch<T> {
    /// Key absent — leave the stored value untouched.
    #[default]
    Keep,
    /// Key present as explicit `null` — clear back to inherit.
    Clear,
    /// Key present with a value — replace the stored value.
    Set(T),
}

/// Serde only invokes a field deserializer when the key is present, so
/// a present key maps to [`Patch::Clear`] or [`Patch::Set`] here and
/// `#[serde(default)]` covers absence as [`Patch::Keep`].
fn deserialize_patch<'de, D, T>(deserializer: D) -> Result<Patch<T>, D::Error>
where
    D: serde::Deserializer<'de>,
    T: serde::Deserialize<'de>,
{
    Ok(Option::<T>::deserialize(deserializer)?.map_or(Patch::Clear, Patch::Set))
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct DeleteTargetParams {
    pub slug: String,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct SetGoalsParams {
    pub slug: String,
    pub goals: Vec<GoalWire>,
}

#[tool_router(router = tool_router_targets, vis = "pub")]
impl McpHandler {
    #[tool(description = "Create or upsert a target. Supply exactly one of \
                       catalog_ref (resolved via the embedded catalog) or \
                       display_name + ra_hours + dec_degrees. The slug is \
                       derived from catalog_ref, or a kebab-cased \
                       display_name, and resolved against the store: \
                       absent -> created; present and the same object \
                       (coordinates within ~10 arcmin) -> in-place update \
                       (created: false, slug unchanged); present and a \
                       different object -> a suffixed slug is allocated. \
                       goals[] defaults to targets.default_goals from \
                       config when omitted; every goal's filter must be \
                       in the connected rig's configured filter roster. \
                       scheduling overrides fall back field-wise to \
                       targets.default_scheduling from config when \
                       omitted (Decision 9 — altitude-gating parity). \
                       position_angle_degrees is the framing angle in \
                       degrees east of north (at least 0.0, below 360.0); \
                       omitted, the imaging train's configured default \
                       applies at read time. \
                       A third form for importers: bare ra_hours + \
                       dec_degrees + source {kind, client, received_at} — \
                       catalog_ref, display_name, active, notes, and \
                       position_angle_degrees are \
                       all rejected with source; the import lands paused \
                       (active: false), rp names it by reverse cone-search \
                       against the catalog, dedup is proximity-only \
                       (target_store.import.dedup_arcsec), and only a \
                       still-pending row last written by the same \
                       source.kind is ever upserted in place.")]
    pub(crate) async fn add_target(
        &self,
        Parameters(params): Parameters<AddTargetParams>,
    ) -> Result<CallToolResult, rmcp::ErrorData> {
        let Some(store) = self.target_store.as_ref() else {
            return Ok(tool_error!("target store not configured"));
        };

        if let Some(source) = &params.source {
            return self.add_target_import(&params, source).await;
        }

        // Every payload rule, in one place, shared with `validate_plan`
        // (rp.md § Plan schema and validation). A writer stops at the
        // first offending field; the structured list is what the
        // read-only tool reports. The per-field parsing below re-runs
        // the same checks as it produces values — belt and braces, and
        // the reason those branches are unreachable rather than dead.
        let payload_errors = super::plan_validation::validate_add_target(
            &params,
            &self.target_store_defaults.default_goals,
            &self.equipment,
        );
        if !payload_errors.is_empty() {
            return Ok(tool_error!("{}", render_first(&payload_errors)));
        }

        let (
            display_name,
            ra_hours,
            dec_degrees,
            catalog_ref,
            object_type,
            magnitude,
            size_arcmin,
            base_slug_input,
        ) = match (&params.catalog_ref, &params.display_name) {
            (Some(_), Some(_)) | (None, None) => {
                return Ok(tool_error!(
                    "supply exactly one of catalog_ref or display_name"
                ))
            }
            (Some(cat), None) => {
                let resolved = match crate::planner::catalog::resolve(cat) {
                    crate::planner::catalog::ResolveOutcome::Resolved(v) => v,
                    crate::planner::catalog::ResolveOutcome::NotFound { suggestions } => {
                        return Ok(CallToolResult::error(vec![ContentBlock::text(
                            json!({
                                "error": "target_not_found",
                                "name": cat,
                                "suggestions": suggestions,
                            })
                            .to_string(),
                        )]));
                    }
                };
                let (ra, dec) = match (params.ra_hours, params.dec_degrees) {
                    (Some(ra), Some(dec)) => (ra, dec),
                    (None, None) => (resolved.ra_hours, resolved.dec_degrees),
                    _ => {
                        return Ok(tool_error!(
                            "ra_hours and dec_degrees must both be supplied together"
                        ))
                    }
                };
                (
                    resolved.name.clone(),
                    ra,
                    dec,
                    Some(resolved.name.clone()),
                    Some(resolved.object_type.clone()),
                    resolved.magnitude,
                    resolved.size_arcmin,
                    resolved.name,
                )
            }
            (None, Some(name)) => {
                let (Some(ra), Some(dec)) = (params.ra_hours, params.dec_degrees) else {
                    return Ok(tool_error!(
                        "the display_name form requires ra_hours and dec_degrees"
                    ));
                };
                (name.clone(), ra, dec, None, None, None, None, name.clone())
            }
        };

        // Validate coordinates up front — the newtype closes the
        // store-write gap by construction (ADR-019), so an out-of-range
        // add is rejected here before any store I/O.
        let coord = match IcrsCoord::try_new(ra_hours, dec_degrees) {
            Ok(c) => c,
            Err(e) => return Ok(tool_error!("{}", e)),
        };

        let base_slug = match TargetSlug::from_display_name(&base_slug_input) {
            Ok(s) => s,
            Err(e) => return Ok(tool_error!("{}", e)),
        };

        let existing = match store.get_target(&base_slug).await {
            Ok(v) => v,
            Err(e) => return Ok(tool_error!("target store error: {}", e)),
        };
        let (final_slug, created) = match existing {
            None => (base_slug, true),
            Some(existing)
                if same_object(
                    ra_hours,
                    dec_degrees,
                    existing.coord.ra_hours(),
                    existing.coord.dec_degrees(),
                ) =>
            {
                (base_slug, false)
            }
            Some(_) => match allocate_suffix(store.as_ref(), &base_slug).await {
                Ok(s) => (s, true),
                Err(e) => return Ok(tool_error!("target store error: {}", e)),
            },
        };

        let goals = match parse_goals(
            params.goals.as_deref(),
            &self.target_store_defaults.default_goals,
            &self.equipment,
        ) {
            Ok(g) => g,
            Err(e) => return Ok(tool_error!("{}", e)),
        };
        let position_angle_degrees = match validate_position_angle(params.position_angle_degrees) {
            Ok(v) => v,
            Err(e) => return Ok(tool_error!("{}", e)),
        };

        let now = chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Millis, true);
        let target = Target {
            slug: final_slug,
            display_name,
            coord,
            catalog_ref,
            object_type,
            magnitude,
            size_arcmin,
            position_angle_degrees,
            priority: 0,
            active: params.active.unwrap_or(true),
            goals,
            scheduling: params.scheduling.map(Into::into),
            grading: params.grading.map(Into::into),
            notes: params.notes,
            created_at: now.clone(),
            updated_at: now,
            created_by: OPERATOR_WRITER.to_string(),
            updated_by: OPERATOR_WRITER.to_string(),
        };
        if let Err(e) = store.upsert_target(target.clone()).await {
            return Ok(tool_error!("target store error: {}", e));
        }

        Ok(tool_success!({
            "slug": target.slug.as_str(),
            "created": created,
            "target": target_to_json(&target),
        }))
    }

    #[tool(description = "Fetch one target with progress derived from the \
                       frames on disk (per-goal {filter, binning, \
                       exposure_duration, desired_count, good, total}).")]
    pub(crate) async fn get_target(
        &self,
        Parameters(params): Parameters<GetTargetParams>,
    ) -> Result<CallToolResult, rmcp::ErrorData> {
        let Some(store) = self.target_store.as_ref() else {
            return Ok(tool_error!("target store not configured"));
        };
        let slug = match TargetSlug::new(&params.slug) {
            Ok(s) => s,
            Err(e) => return Ok(tool_error!("{}", e)),
        };
        match store.get_target(&slug).await {
            Ok(Some(t)) => {
                let counts = self.derive_progress(&t).await;
                Ok(tool_success!({
                    "target": target_to_json(&t),
                    "progress": progress_rows(&t, &counts),
                }))
            }
            Ok(None) => Ok(tool_error!("no target with slug {:?}", params.slug)),
            Err(e) => Ok(tool_error!("target store error: {}", e)),
        }
    }

    #[tool(description = "List all targets, each with derived progress, \
                       optionally filtered to active == true.")]
    pub(crate) async fn list_targets(
        &self,
        Parameters(params): Parameters<ListTargetsParams>,
    ) -> Result<CallToolResult, rmcp::ErrorData> {
        let Some(store) = self.target_store.as_ref() else {
            return Ok(tool_error!("target store not configured"));
        };
        let mut targets = match store.list_targets().await {
            Ok(v) => v,
            Err(e) => return Ok(tool_error!("target store error: {}", e)),
        };
        if params.active_only == Some(true) {
            targets.retain(|t| t.active);
        }
        let mut items: Vec<Value> = Vec::with_capacity(targets.len());
        for t in &targets {
            let counts = self.derive_progress(t).await;
            let mut v = target_to_json(t);
            // `target_to_json` returns an object literal, so
            // `as_object_mut` always succeeds.
            let map = v.as_object_mut();
            debug_assert!(map.is_some(), "target JSON must be an object");
            if let Some(map) = map {
                map.insert("progress".to_owned(), json!(progress_rows(t, &counts)));
            }
            items.push(v);
        }
        Ok(tool_success!({ "targets": items }))
    }

    #[tool(description = "Edit a target's fields in place. Does not touch \
                       the slug or on-disk frames. Setting active: true is \
                       how a pending target is accepted into rotation. \
                       scheduling, when supplied, replaces the whole \
                       overrides object (not a field-wise merge). \
                       position_angle_degrees set to an explicit null \
                       clears the framing angle back to \
                       inherit-the-train-default; omitted it is untouched.")]
    pub(crate) async fn update_target(
        &self,
        Parameters(params): Parameters<UpdateTargetParams>,
    ) -> Result<CallToolResult, rmcp::ErrorData> {
        let Some(store) = self.target_store.as_ref() else {
            return Ok(tool_error!("target store not configured"));
        };
        let slug = match TargetSlug::new(&params.slug) {
            Ok(s) => s,
            Err(e) => return Ok(tool_error!("{}", e)),
        };
        let mut target = match store.get_target(&slug).await {
            Ok(Some(t)) => t,
            Ok(None) => return Ok(tool_error!("no target with slug {:?}", params.slug)),
            Err(e) => return Ok(tool_error!("target store error: {}", e)),
        };
        if let Some(v) = params.display_name {
            target.display_name = v;
        }
        // Re-validate through the newtype whenever either coordinate moves,
        // so an out-of-range edit is rejected (ADR-019 store-write gap).
        if params.ra_hours.is_some() || params.dec_degrees.is_some() {
            let ra = params.ra_hours.unwrap_or_else(|| target.coord.ra_hours());
            let dec = params
                .dec_degrees
                .unwrap_or_else(|| target.coord.dec_degrees());
            match IcrsCoord::try_new(ra, dec) {
                Ok(c) => target.coord = c,
                Err(e) => return Ok(tool_error!("{}", e)),
            }
        }
        if let Some(v) = params.active {
            target.active = v;
        }
        if let Some(v) = params.priority {
            target.priority = v;
        }
        if let Some(v) = params.scheduling {
            let errors = super::plan_validation::validate_scheduling(&v, "scheduling");
            if !errors.is_empty() {
                return Ok(tool_error!("{}", render_first(&errors)));
            }
            target.scheduling = Some(v.into());
        }
        if params.notes.is_some() {
            target.notes = params.notes;
        }
        // Absent → untouched, explicit null → cleared back to inherit,
        // a number → validated and set (rp.md § Target Store →
        // Position angle).
        match params.position_angle_degrees {
            Patch::Keep => {}
            Patch::Clear => target.position_angle_degrees = None,
            Patch::Set(v) => match validate_position_angle(Some(v)) {
                Ok(v) => target.position_angle_degrees = v,
                Err(e) => return Ok(tool_error!("{}", e)),
            },
        }
        // Same tri-state shape: absent → untouched, explicit null →
        // cleared back to inherit `target_store.default_grading`, an
        // object → replaces the overrides wholesale.
        match params.grading {
            Patch::Keep => {}
            Patch::Clear => target.grading = None,
            Patch::Set(g) => {
                let errors = super::plan_validation::validate_grading(&g, "grading");
                if !errors.is_empty() {
                    return Ok(tool_error!("{}", render_first(&errors)));
                }
                target.grading = Some(g.into());
            }
        }
        target.updated_at = chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Millis, true);
        target.updated_by = OPERATOR_WRITER.to_string();
        if let Err(e) = store.upsert_target(target.clone()).await {
            return Ok(tool_error!("target store error: {}", e));
        }
        Ok(tool_success!({ "target": target_to_json(&target) }))
    }

    #[tool(description = "Remove a target's plan row (deleted: false for \
                       an absent slug). Frames already captured under the \
                       slug are left untouched on disk.")]
    pub(crate) async fn delete_target(
        &self,
        Parameters(params): Parameters<DeleteTargetParams>,
    ) -> Result<CallToolResult, rmcp::ErrorData> {
        let Some(store) = self.target_store.as_ref() else {
            return Ok(tool_error!("target store not configured"));
        };
        let slug = match TargetSlug::new(&params.slug) {
            Ok(s) => s,
            Err(e) => return Ok(tool_error!("{}", e)),
        };
        match store.delete_target(&slug).await {
            Ok(deleted) => Ok(tool_success!({ "deleted": deleted })),
            Err(e) => Ok(tool_error!("target store error: {}", e)),
        }
    }

    #[tool(description = "Replace a target's goal set atomically (not a \
                       merge). Same filter-roster validation as \
                       add_target.")]
    pub(crate) async fn set_goals(
        &self,
        Parameters(params): Parameters<SetGoalsParams>,
    ) -> Result<CallToolResult, rmcp::ErrorData> {
        let Some(store) = self.target_store.as_ref() else {
            return Ok(tool_error!("target store not configured"));
        };
        let slug = match TargetSlug::new(&params.slug) {
            Ok(s) => s,
            Err(e) => return Ok(tool_error!("{}", e)),
        };
        let goals = match super::plan_validation::validate_goals(
            &params.goals,
            &super::plan_validation::filter_roster(&self.equipment),
            "goals",
        ) {
            Ok(g) => g,
            Err(errors) => return Ok(tool_error!("{}", render_first(&errors))),
        };
        let stamp = WriteStamp {
            updated_at: chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Millis, true),
            updated_by: OPERATOR_WRITER.to_string(),
        };
        if let Err(e) = store.set_goals(&slug, goals, stamp).await {
            return Ok(tool_error!("target store error: {}", e));
        }
        match store.get_target(&slug).await {
            Ok(Some(t)) => Ok(tool_success!({ "target": target_to_json(&t) })),
            Ok(None) => Ok(tool_error!("no target with slug {:?}", params.slug)),
            Err(e) => Ok(tool_error!("target store error: {}", e)),
        }
    }
}

impl McpHandler {
    /// `add_target`'s import form (rp.md § Target Store → Import form):
    /// bare coordinates plus typed provenance. Naming is rp's job here —
    /// reverse cone-search against the catalog — identity is proximity-only
    /// dedup, and the import always lands paused.
    async fn add_target_import(
        &self,
        params: &AddTargetParams,
        source: &SourceWire,
    ) -> Result<CallToolResult, rmcp::ErrorData> {
        let Some(store) = self.target_store.as_ref() else {
            return Ok(tool_error!("target store not configured"));
        };
        if let Some(rejected) = reject_import_incompatible_fields(params, source) {
            return Ok(*rejected);
        }
        let (Some(ra_hours), Some(dec_degrees)) = (params.ra_hours, params.dec_degrees) else {
            return Ok(tool_error!(
                "the source form requires ra_hours and dec_degrees"
            ));
        };
        let coord = match IcrsCoord::try_new(ra_hours, dec_degrees) {
            Ok(c) => c,
            Err(e) => return Ok(tool_error!("{}", e)),
        };
        let goals = match parse_goals(
            params.goals.as_deref(),
            &self.target_store_defaults.default_goals,
            &self.equipment,
        ) {
            Ok(g) => g,
            Err(e) => return Ok(tool_error!("{}", e)),
        };

        let import = &self.target_store_defaults.import;
        let all = match store.list_targets().await {
            Ok(v) => v,
            Err(e) => return Ok(tool_error!("target store error: {}", e)),
        };
        let now = chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Millis, true);

        // Identity: proximity-only dedup — the nearest stored row within
        // dedup_arcsec, whatever its name. The catalog_ref-match branch of
        // slug allocation is never consulted for imports: two framings of
        // the same object beyond tolerance are two targets (mosaic panels).
        let dedup_degrees = import.dedup_arcsec / 3600.0;
        let neighbor = all
            .iter()
            .map(|t| {
                (
                    t,
                    flat_separation_degrees(
                        ra_hours,
                        dec_degrees,
                        t.coord.ra_hours(),
                        t.coord.dec_degrees(),
                    ),
                )
            })
            .filter(|(_, separation)| *separation < dedup_degrees)
            .min_by(|a, b| a.1.total_cmp(&b.1))
            .map(|(t, _)| t);

        if let Some(existing) = neighbor {
            if let Some(refreshed) =
                refresh_import_in_place(store.as_ref(), existing, coord, source, &now).await
            {
                return refreshed;
            }
            // Active, operator-edited, or operator-created: the row is
            // never modified — fall through and create a new pending
            // target beside it (Decision 3's protection, enforced here).
        }

        let naming = self.import_naming(&coord, ra_hours, dec_degrees, &all);

        let base_slug = match TargetSlug::from_display_name(&naming.base_slug_input) {
            Ok(s) => s,
            Err(e) => return Ok(tool_error!("{}", e)),
        };
        // Imports never take over an existing slug — dedup above already
        // decided this is a distinct target, so a collision always
        // suffix-allocates.
        let final_slug = match store.get_target(&base_slug).await {
            Ok(None) => base_slug,
            Ok(Some(_)) => match allocate_suffix(store.as_ref(), &base_slug).await {
                Ok(s) => s,
                Err(e) => return Ok(tool_error!("target store error: {}", e)),
            },
            Err(e) => return Ok(tool_error!("target store error: {}", e)),
        };

        let target = Target {
            slug: final_slug,
            display_name: naming.display_name,
            coord,
            catalog_ref: naming.catalog_ref,
            object_type: naming.object_type,
            magnitude: naming.magnitude,
            size_arcmin: naming.size_arcmin,
            // Imports never carry a framing angle — the operator enters
            // one in the P4 inbox; until then the train default applies.
            position_angle_degrees: None,
            priority: 0,
            active: false,
            goals,
            scheduling: params.scheduling.map(Into::into),
            grading: None,
            notes: Some(provenance_line(source)),
            created_at: now.clone(),
            updated_at: now,
            created_by: source.kind.clone(),
            updated_by: source.kind.clone(),
        };
        if let Err(e) = store.upsert_target(target.clone()).await {
            return Ok(tool_error!("target store error: {}", e));
        }
        Ok(tool_success!({
            "slug": target.slug.as_str(),
            "created": true,
            "target": target_to_json(&target),
        }))
    }

    /// Naming for an import: one reverse cone-search over the one
    /// logical catalog; coverage bounds naming quality only, never
    /// import correctness.
    fn import_naming(
        &self,
        coord: &IcrsCoord,
        ra_hours: f64,
        dec_degrees: f64,
        all: &[Target],
    ) -> ImportNaming {
        let import = &self.target_store_defaults.import;
        let tolerances = rp_catalog::NearestTolerances {
            dso_arcmin: import.naming_tolerance_arcmin,
            star_arcmin: import.star_naming_tolerance_arcmin,
        };
        if let Some(hit) = crate::planner::catalog::nearest(coord, &tolerances) {
            let name = hit.target.name.clone();
            // The plain name is reserved for a dead-center import
            // of an object no stored target already claims; any
            // other hit reads as *how this framing differs*.
            let sole_claim = !all
                .iter()
                .any(|t| t.catalog_ref.as_deref() == Some(name.as_str()));
            let centered = hit.separation_arcmin * 60.0 <= import.dedup_arcsec;
            let display = if sole_claim && centered {
                name.clone()
            } else {
                offset_display_name(&name, hit.east_offset_arcmin, hit.north_offset_arcmin)
            };
            ImportNaming {
                display_name: display,
                catalog_ref: Some(name.clone()),
                object_type: Some(hit.target.object_type.clone()),
                magnitude: hit.target.magnitude,
                size_arcmin: hit.target.size_arcmin,
                base_slug_input: name,
            }
        } else {
            let (display, slug) = coordinate_forms(ra_hours, dec_degrees);
            ImportNaming {
                display_name: display,
                catalog_ref: None,
                object_type: None,
                magnitude: None,
                size_arcmin: None,
                base_slug_input: slug,
            }
        }
    }
}

/// What the reverse cone-search names an import: the display name,
/// catalog linkage, and the slug seed.
struct ImportNaming {
    display_name: String,
    catalog_ref: Option<String>,
    object_type: Option<String>,
    magnitude: Option<f64>,
    size_arcmin: Option<f64>,
    base_slug_input: String,
}

/// Decision 3's refresh-in-place: a still-pending row unedited since
/// the same importer wrote it takes new coordinates and provenance;
/// slug, `display_name`, and goals stay. `None` when the row is
/// protected — the caller creates a new pending target beside it.
async fn refresh_import_in_place(
    store: &dyn TargetStore,
    existing: &Target,
    coord: IcrsCoord,
    source: &SourceWire,
    now: &str,
) -> Option<Result<CallToolResult, rmcp::ErrorData>> {
    if existing.active || existing.updated_by != source.kind {
        return None;
    }
    let mut target = existing.clone();
    target.coord = coord;
    target.notes = Some(provenance_line(source));
    target.updated_at = now.to_string();
    target.updated_by = source.kind.clone();
    if let Err(e) = store.upsert_target(target.clone()).await {
        return Some(Ok(tool_error!("target store error: {}", e)));
    }
    Some(Ok(tool_success!({
        "slug": target.slug.as_str(),
        "created": false,
        "target": target_to_json(&target),
    })))
}

/// The import form rejects every field rp derives itself (rp.md
/// § Target Store → Import form); each rejection explains where the
/// value comes from instead. Also rejects a writer identity that is
/// empty or claims to be the operator.
fn reject_import_incompatible_fields(
    params: &AddTargetParams,
    source: &SourceWire,
) -> Option<Box<CallToolResult>> {
    if params.catalog_ref.is_some() || params.display_name.is_some() {
        return Some(Box::new(tool_error!(
            "catalog_ref and display_name are not accepted with source — rp names imports itself"
        )));
    }
    if params.active.is_some() {
        return Some(Box::new(tool_error!(
            "active is not accepted with source — imports always land paused (active: false)"
        )));
    }
    if params.position_angle_degrees.is_some() {
        return Some(Box::new(tool_error!(
            "position_angle_degrees is not accepted with source — imports \
             never carry a framing angle (it is entered by the operator)"
        )));
    }
    if params.notes.is_some() {
        return Some(Box::new(tool_error!(
            "notes is not accepted with source — rp writes the provenance line itself"
        )));
    }
    if params.grading.is_some() {
        return Some(Box::new(tool_error!(
            "grading is not accepted with source — imports inherit \
             target_store.default_grading until the operator sets thresholds"
        )));
    }
    if source.kind.trim().is_empty() || source.kind == OPERATOR_WRITER {
        return Some(Box::new(tool_error!(
            "source.kind must be a non-empty writer identity other than {:?}",
            OPERATOR_WRITER
        )));
    }
    None
}

/// Boundary validation for the per-target framing angle, sharing the
/// domain (and validator) of the per-train config default
/// ([`crate::config::PositionAngleDegrees`]), with the error naming
/// the tool parameter.
fn validate_position_angle(value: Option<f64>) -> Result<Option<f64>, String> {
    let errors = super::plan_validation::validate_position_angle(value, "position_angle_degrees");
    if errors.is_empty() {
        Ok(value)
    } else {
        Err(render_first(&errors))
    }
}

/// Renders the first [`FieldError`] of a validation pass into the flat
/// string the write tools have always returned. The structured list is
/// what `validate_plan` reports; a writer stops at the first offending
/// field, so it only ever needs this one.
fn render_first(errors: &[rusty_photon_config::actions::FieldError]) -> String {
    errors
        .first()
        .map_or_else(String::new, |e| format!("{}: {}", e.path, e.msg))
}

fn same_object(ra1_hours: f64, dec1_degrees: f64, ra2_hours: f64, dec2_degrees: f64) -> bool {
    flat_separation_degrees(ra1_hours, dec1_degrees, ra2_hours, dec2_degrees)
        < DEDUP_TOLERANCE_DEGREES
}

/// Flat, `cos(dec)`-weighted angular separation in degrees, wrap-correct
/// in RA. At arcsecond-to-arcminute dedup scales the flat approximation
/// is indistinguishable from the great-circle distance.
fn flat_separation_degrees(
    ra1_hours: f64,
    dec1_degrees: f64,
    ra2_hours: f64,
    dec2_degrees: f64,
) -> f64 {
    let mut ra_diff_hours = (ra1_hours - ra2_hours).rem_euclid(24.0);
    if ra_diff_hours > 12.0 {
        ra_diff_hours -= 24.0;
    }
    let ra_deg_diff = ra_diff_hours * 15.0 * dec1_degrees.to_radians().cos();
    let dec_diff = dec1_degrees - dec2_degrees;
    ra_deg_diff.hypot(dec_diff)
}

fn parse_goals(
    wire: Option<&[GoalWire]>,
    defaults: &[AcquisitionGoal],
    equipment: &EquipmentRegistry,
) -> Result<Vec<AcquisitionGoal>, String> {
    wire.map_or_else(
        || Ok(defaults.to_vec()),
        |wire_goals| {
            super::plan_validation::validate_goals(
                wire_goals,
                &super::plan_validation::filter_roster(equipment),
                "goals",
            )
            .map_err(|errors| render_first(&errors))
        },
    )
}

/// The human-readable provenance line an import writes into `notes` —
/// display data, never parsed (Decision 3). Refreshed wholesale on an
/// in-place import upsert, which is safe because only rows the importer
/// itself last wrote are ever upserted.
fn provenance_line(source: &SourceWire) -> String {
    format!(
        "Imported via {} from {} at {}",
        source.kind, source.client, source.received_at
    )
}

/// The offset display form for a framed import of a catalog object —
/// `"NGC 7000 +8′E −4′N"`: East = Δα·cos δ and North = Δδ of the framing
/// from the catalog centroid, reading as *how this framing differs*.
fn offset_display_name(base: &str, east_arcmin: f64, north_arcmin: f64) -> String {
    let mut name = base.to_string();
    for (value, axis) in [(east_arcmin, 'E'), (north_arcmin, 'N')] {
        if let Some(component) = offset_component(value, axis) {
            name.push(' ');
            name.push_str(&component);
        }
    }
    name
}

/// One offset component, rendered to 0.1′ with a trailing `.0` stripped
/// (`+8′E`, `+0.3′E`); `None` under 0.05′. Typography matches the design
/// doc: U+2032 prime, U+2212 minus.
fn offset_component(value_arcmin: f64, axis: char) -> Option<String> {
    let magnitude = value_arcmin.abs();
    if magnitude < 0.05 {
        return None;
    }
    let sign = if value_arcmin < 0.0 { '\u{2212}' } else { '+' };
    let rendered = format!("{magnitude:.1}");
    let rendered = rendered.strip_suffix(".0").unwrap_or(&rendered);
    Some(format!("{sign}{rendered}\u{2032}{axis}"))
}

/// The display name and slug for an import with no catalog neighbor:
/// IAU-style truncation `Jhhmm±ddmm` (`"J2059+4432"`) and its matching
/// slug shape (`"j2059p4432"`, `p`/`m` for the sign).
fn coordinate_forms(ra_hours: f64, dec_degrees: f64) -> (String, String) {
    let (ra_hh, ra_mm) = truncated_minutes(ra_hours);
    let ra_hh = ra_hh.min(23); // IcrsCoord bounds RA to [0, 24)
    let (dec_dd, dec_mm) = truncated_minutes(dec_degrees.abs());
    let display_sign = if dec_degrees < 0.0 { '-' } else { '+' };
    let slug_sign = if dec_degrees < 0.0 { 'm' } else { 'p' };
    (
        format!("J{ra_hh:02}{ra_mm:02}{display_sign}{dec_dd:02}{dec_mm:02}"),
        format!("j{ra_hh:02}{ra_mm:02}{slug_sign}{dec_dd:02}{dec_mm:02}"),
    )
}

/// Splits a non-negative value into truncated `(whole, minutes)` — the
/// IAU convention truncates rather than rounds. Decimal inputs like
/// `1.9` h sit a hair *below* the exact minute boundary in binary
/// floating point (`1.9 * 60` = 53.999…96), so round to a micro-minute
/// first: truncation then reflects the intended coordinate, not its
/// representation.
#[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
#[expect(
    clippy::as_conversions,
    reason = "coordinates are bounded (RA < 1,440 arc-minutes, |dec| <= 5,400), far inside u32; no total f64-to-u32 spelling exists"
)]
fn truncated_minutes(value: f64) -> (u32, u32) {
    let total_minutes = ((value * 60.0 * 1e6).round() / 1e6) as u32;
    (total_minutes / 60, total_minutes % 60)
}

/// Lowest unused `"{base}-{n}"` for `n` from 2. Terminates by the
/// pigeonhole principle (rp-targets.md § Slug allocation): the store
/// holds finitely many targets, so some suffix is always free.
async fn allocate_suffix(store: &dyn TargetStore, base: &TargetSlug) -> Result<TargetSlug, String> {
    let mut n: u32 = 2;
    loop {
        let candidate = TargetSlug::new(&format!("{base}-{n}")).map_err(|e| e.to_string())?;
        match store.get_target(&candidate).await {
            Ok(None) => return Ok(candidate),
            Ok(Some(_)) => {
                n = n
                    .checked_add(1)
                    .ok_or_else(|| "slug suffix space exhausted".to_string())?;
            }
            Err(e) => return Err(e.to_string()),
        }
    }
}

/// The MCP wire shape of one target is [`Target`]'s **derived**
/// `Serialize` (rp.md § Target MCP tools) — no hand-keyed re-statement.
/// The derived shape and the [`GoalWire`] input shape agree because
/// `Binning` serializes as its canonical `"AxB"` string and
/// `exposure_duration` as humantime; `grading` rides along as `null`
/// on a target that inherits `target_store.default_grading` rather than
/// overriding it.
fn target_to_json(t: &Target) -> Value {
    // Infallible for `Target` (string map keys, infallible Serialize
    // impls); `Null` could only surface a serializer bug.
    serde_json::to_value(t).unwrap_or(Value::Null)
}

/// One per-goal progress row (rp.md § Progress derivation): the goal's
/// wire shape plus the scan counters. `#[serde(flatten)]` embeds
/// [`GoalWire`] verbatim — one declaration, no re-keyed copy — so a
/// row's goal part is byte-for-byte the `goals[]` shape `set_goals`
/// accepts (including the `desired_count` key).
#[derive(Debug, Serialize)]
pub(crate) struct ProgressRow {
    #[serde(flatten)]
    goal: GoalWire,
    /// Frames captured against this goal's sub-spec that grading did
    /// not demonstrably reject.
    good: u32,
    /// Every frame captured against this goal's sub-spec.
    total: u32,
}

impl McpHandler {
    /// Derive one target's per-goal counts from the frames on disk
    /// (rp.md § Progress derivation), resolving its effective grading
    /// thresholds against `target_store.default_grading` first.
    ///
    /// `pub(crate)` because the planner tools derive the same numbers —
    /// there is exactly one definition of a target's progress, and both
    /// the operator surface and `get_next_target` read it here.
    pub(crate) async fn derive_progress(
        &self,
        target: &Target,
    ) -> Vec<crate::planner::progress_scan::GoalProgress> {
        let thresholds = crate::planner::progress_scan::effective_thresholds(
            target,
            self.target_store_defaults.default_grading,
        );
        crate::planner::progress_scan::scan_target(
            std::path::Path::new(&self.session_config.data_directory),
            self.naming_templates.as_deref(),
            target,
            thresholds,
        )
        .await
    }
}

/// Pair a target's goals with the counts the on-disk scan derived for
/// them. `counts` comes back from
/// [`crate::planner::progress_scan::scan_target`] in goal order, so the
/// zip is positional; a short `counts` (only possible if the two ever
/// disagreed about goal count) reports zeros rather than panicking.
pub(crate) fn progress_rows(
    t: &Target,
    counts: &[crate::planner::progress_scan::GoalProgress],
) -> Vec<ProgressRow> {
    t.goals
        .iter()
        .enumerate()
        .map(|(i, g)| {
            let counts = counts.get(i).copied().unwrap_or_default();
            ProgressRow {
                goal: GoalWire::from(g),
                good: counts.good,
                total: counts.total,
            }
        })
        .collect()
}

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
mod tests {
    use super::*;

    #[test]
    fn offset_component_renders_to_tenth_arcmin_with_trailing_zero_stripped() {
        assert_eq!(offset_component(8.0, 'E').unwrap(), "+8\u{2032}E");
        assert_eq!(offset_component(-4.0, 'N').unwrap(), "\u{2212}4\u{2032}N");
        assert_eq!(offset_component(0.3, 'E').unwrap(), "+0.3\u{2032}E");
        assert_eq!(offset_component(12.34, 'N').unwrap(), "+12.3\u{2032}N");
    }

    #[test]
    fn offset_component_omits_under_a_twentieth_arcmin() {
        assert_eq!(offset_component(0.04, 'E'), None);
        assert_eq!(offset_component(-0.04, 'N'), None);
    }

    #[test]
    fn offset_display_name_matches_the_design_doc_example() {
        assert_eq!(
            offset_display_name("NGC 7000", 8.0, -4.0),
            "NGC 7000 +8\u{2032}E \u{2212}4\u{2032}N"
        );
    }

    #[test]
    fn offset_display_name_degenerates_to_the_plain_name() {
        assert_eq!(offset_display_name("NGC 7000", 0.01, -0.02), "NGC 7000");
    }

    #[test]
    fn coordinate_forms_truncate_iau_style() {
        // The design doc's own example shape: Jhhmm±ddmm, truncated.
        assert_eq!(
            coordinate_forms(20.99, 44.54),
            ("J2059+4432".to_string(), "j2059p4432".to_string())
        );
        assert_eq!(
            coordinate_forms(5.5, -0.75),
            ("J0530-0045".to_string(), "j0530m0045".to_string())
        );
    }

    #[test]
    fn flat_separation_is_wrap_correct_in_ra() {
        let separation = flat_separation_degrees(23.999, 0.0, 0.001, 0.0);
        assert!(separation < 0.05, "wrapped separation was {separation}");
    }

    #[test]
    fn provenance_line_names_kind_client_and_receipt_time() {
        let source = SourceWire {
            kind: "planetarium-bridge".to_string(),
            client: "192.0.2.10:52441".to_string(),
            received_at: "2026-07-30T01:23:45.678Z".to_string(),
        };
        assert_eq!(
            provenance_line(&source),
            "Imported via planetarium-bridge from 192.0.2.10:52441 at 2026-07-30T01:23:45.678Z"
        );
    }

    // Pins the empty-sky fixture point the import BDD suite uses for the
    // coordinate-form naming scenario: no deep-sky object within 10' and
    // no star within 2' at the default tolerances. If a catalog expansion
    // ever populates this spot, move both this test and the scenario.
    #[test]
    fn empty_sky_fixture_point_has_no_catalog_neighbor() {
        let coord = IcrsCoord::try_new(1.9, -44.9).unwrap();
        let tolerances = rp_catalog::NearestTolerances {
            dso_arcmin: 10.0,
            star_arcmin: 2.0,
        };
        assert_eq!(crate::planner::catalog::nearest(&coord, &tolerances), None);
    }

    // The tri-state contract of update_target's
    // position_angle_degrees (rp.md § Target Store → Position angle):
    // absent → untouched, explicit null → clear, number → set.
    #[test]
    fn update_params_distinguish_absent_null_and_value_position_angles() {
        let absent: UpdateTargetParams = serde_json::from_value(json!({ "slug": "m31" })).unwrap();
        assert_eq!(absent.position_angle_degrees, Patch::Keep);

        let cleared: UpdateTargetParams =
            serde_json::from_value(json!({ "slug": "m31", "position_angle_degrees": null }))
                .unwrap();
        assert_eq!(cleared.position_angle_degrees, Patch::Clear);

        let set: UpdateTargetParams =
            serde_json::from_value(json!({ "slug": "m31", "position_angle_degrees": 254.5 }))
                .unwrap();
        assert_eq!(set.position_angle_degrees, Patch::Set(254.5));
    }

    #[test]
    fn position_angle_validation_bounds_the_move_rotator_domain() {
        assert_eq!(validate_position_angle(None).unwrap(), None);
        assert_eq!(validate_position_angle(Some(0.0)).unwrap(), Some(0.0));
        assert_eq!(
            validate_position_angle(Some(359.999)).unwrap(),
            Some(359.999)
        );
        for bad in [360.0, -0.001, f64::NAN, f64::INFINITY] {
            let err = validate_position_angle(Some(bad)).unwrap_err();
            assert!(err.contains("position_angle_degrees"), "{err}");
        }
    }
}
