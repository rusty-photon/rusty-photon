use rmcp::handler::server::wrapper::Parameters;
use rmcp::model::{CallToolResult, ContentBlock};
use rmcp::{tool, tool_router};
use schemars::JsonSchema;
use serde::Deserialize;

use super::super::handler::McpHandler;
use super::super::{tool_error, tool_success};

#[derive(Debug, Deserialize, JsonSchema)]
pub struct ResolveTargetParams {
    pub name: String,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct AltAzParams {
    pub ra: f64,
    pub dec: f64,
    #[serde(default)]
    pub time: Option<String>,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct TransitParams {
    pub ra: f64,
    pub dec: f64,
    pub date: String,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct RiseSetParams {
    pub ra: f64,
    pub dec: f64,
    pub date: String,
    pub min_alt_degrees: f64,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct MeridianFlipParams {
    pub ra: f64,
    pub dec: f64,
    #[serde(default)]
    pub time: Option<String>,
    /// One of `"east"`, `"west"`, `"unknown"`. v1 ignores the value
    /// (the meridian-flip primitive is symmetric in side-of-pier) but
    /// requires the field be present, with a `"unknown"` default.
    #[serde(default = "default_side_of_pier")]
    pub side_of_pier: String,
}

fn default_side_of_pier() -> String {
    "unknown".to_string()
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct TimeOnlyParams {
    #[serde(default)]
    pub time: Option<String>,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct TwilightParams {
    pub date: String,
    /// One of `"civil"`, `"nautical"`, `"astronomical"`.
    pub kind: String,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct MoonSeparationParams {
    pub ra: f64,
    pub dec: f64,
    #[serde(default)]
    pub time: Option<String>,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct GetTargetStatusParams {
    /// Catalog name or alias. Mutually exclusive with `ra`+`dec`.
    #[serde(default)]
    pub target_name: Option<String>,
    /// Direct ICRS RA in decimal hours. Must be paired with `dec`.
    #[serde(default)]
    pub ra: Option<f64>,
    /// Direct ICRS Dec in decimal degrees. Must be paired with `ra`.
    #[serde(default)]
    pub dec: Option<f64>,
    #[serde(default)]
    pub time: Option<String>,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct GetNextTargetParams {
    #[serde(default)]
    pub time: Option<String>,
    /// The imaging train, for the effective-position-angle fallback
    /// (rp.md § Target Store → Position angle): a recommended target
    /// without its own angle inherits this train's
    /// `default_position_angle_degrees`. Unknown ids are an error;
    /// omitted, the train-default layer is skipped (target value →
    /// 0.0 north-up).
    #[serde(default)]
    pub train_id: Option<String>,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct RecordExposureParams {
    /// Slug of an active target-store row — the `target.name` a
    /// `get_next_target` recommendation carries (a target's slug is its
    /// stable identity).
    pub target: String,
    /// Filter of the completed frame. Omit (or pass `null` / `""`)
    /// for an unfiltered frame.
    #[serde(default)]
    pub filter: Option<String>,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct GetSessionProgressParams {}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct GetSiteParams {}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct GetMeridianStatusParams {
    #[serde(default)]
    pub time: Option<String>,
}

impl McpHandler {
    /// The planner's working set: every active target-store row
    /// projected onto the decision candidate type
    /// ([`crate::planner::decision::PlannerTarget`], whose `name` is the
    /// target's slug). The single source of planner targets since the
    /// legacy `targets[]` config array was retired — `get_next_target`,
    /// `record_exposure`, `get_session_progress`, and `get_target_status`
    /// all read from here. Returns empty when no store is configured or
    /// the listing fails (logged at `debug!`), which surfaces as
    /// `no_targets_configured` / an unknown-target error rather than a
    /// tool failure.
    async fn active_targets(&self) -> Vec<rp_targets::Target> {
        let Some(store) = self.target_store.as_ref() else {
            return Vec::new();
        };
        match store.list_targets().await {
            Ok(mut targets) => {
                targets.retain(|t| t.active);
                targets
            }
            Err(e) => {
                tracing::debug!(error = %e, "target store list_targets failed; planner sees no targets");
                Vec::new()
            }
        }
    }

    /// The candidate set plus the progress snapshot to rank it against:
    /// every active store row projected onto the decision type, and each
    /// one's per-goal counts derived from the frames on disk (rp.md §
    /// Progress derivation).
    ///
    /// Deriving here — once per call, at the tool boundary — is what
    /// keeps `decision::next_target` a pure function of its arguments.
    async fn planner_snapshot(
        &self,
    ) -> (
        Vec<crate::planner::decision::PlannerTarget>,
        crate::planner::progress::PlanProgress,
    ) {
        let targets = self.active_targets().await;
        let mut progress = crate::planner::progress::PlanProgress::default();
        let mut candidates = Vec::with_capacity(targets.len());
        for target in &targets {
            progress.insert(target.slug.as_str(), self.derive_progress(target).await);
            candidates.push(crate::planner::decision::PlannerTarget::from(target));
        }
        (candidates, progress)
    }
}

#[tool_router(router = tool_router_planner, vis = "pub")]
impl McpHandler {
    // -------------------------------------------------------------------
    // Catalog lookup
    // -------------------------------------------------------------------

    #[tool(
        description = "Resolve a deep-sky object name to ICRS coordinates from \
                       the embedded Messier + NGC + IC catalogue. Case- and \
                       whitespace-insensitive; common-name aliases are honoured. \
                       Returns ra_hours / dec_degrees / object_type / magnitude / \
                       size_arcmin on hit, or a structured not-found payload with \
                       the top three fuzzy suggestions on miss."
    )]
    pub(crate) async fn resolve_target(
        &self,
        Parameters(params): Parameters<ResolveTargetParams>,
    ) -> Result<CallToolResult, rmcp::ErrorData> {
        match crate::planner::catalog::resolve(&params.name) {
            crate::planner::catalog::ResolveOutcome::Resolved(view) => Ok(tool_success!({
                "name": view.name,
                "object_type": view.object_type,
                "ra_hours": view.ra_hours,
                "dec_degrees": view.dec_degrees,
                "magnitude": view.magnitude,
                "size_arcmin": view.size_arcmin,
            })),
            crate::planner::catalog::ResolveOutcome::NotFound { suggestions } => {
                // CallToolResult::error carries text content; we embed
                // a small JSON payload so a planner plugin can pick out
                // suggestions without string parsing.
                Ok(CallToolResult::error(vec![ContentBlock::text(
                    serde_json::json!({
                        "error": "target_not_found",
                        "name": params.name,
                        "suggestions": suggestions,
                    })
                    .to_string(),
                )]))
            }
        }
    }

    // -------------------------------------------------------------------
    // Ephemeris primitives — see docs/services/rp.md
    // §"Primitive vs. Convenience MCP Tools"
    // -------------------------------------------------------------------

    #[tool(description = "Topocentric altitude/azimuth for an ICRS target. \
                       Refraction modelled with default amateur conditions. \
                       Requires the deployment's `site` block.")]
    pub(crate) async fn compute_alt_az(
        &self,
        Parameters(params): Parameters<AltAzParams>,
    ) -> Result<CallToolResult, rmcp::ErrorData> {
        let Some(site) = self.site.as_ref() else {
            return Ok(tool_error!(
                "{}",
                crate::planner::primitives::site_required_error()
            ));
        };
        let target = match crate::planner::primitives::validate_icrs(params.ra, params.dec) {
            Ok(c) => c,
            Err(e) => return Ok(tool_error!("{}", e)),
        };
        let time = match crate::planner::primitives::parse_time_or_now(params.time.as_deref()) {
            Ok(t) => t,
            Err(e) => return Ok(tool_error!("{}", e)),
        };
        match crate::planner::primitives::compute_alt_az(site, target, time) {
            Ok(v) => Ok(CallToolResult::success(vec![ContentBlock::text(
                v.to_string(),
            )])),
            Err(e) => Ok(tool_error!("{}", e)),
        }
    }

    #[tool(description = "UT of upper transit on a given UTC date. Requires `site`.")]
    pub(crate) async fn compute_transit(
        &self,
        Parameters(params): Parameters<TransitParams>,
    ) -> Result<CallToolResult, rmcp::ErrorData> {
        let Some(site) = self.site.as_ref() else {
            return Ok(tool_error!(
                "{}",
                crate::planner::primitives::site_required_error()
            ));
        };
        let target = match crate::planner::primitives::validate_icrs(params.ra, params.dec) {
            Ok(c) => c,
            Err(e) => return Ok(tool_error!("{}", e)),
        };
        let date = match crate::planner::primitives::parse_date(&params.date) {
            Ok(d) => d,
            Err(e) => return Ok(tool_error!("{}", e)),
        };
        let v = crate::planner::primitives::compute_transit(site, target, date);
        Ok(CallToolResult::success(vec![ContentBlock::text(
            v.to_string(),
        )]))
    }

    #[tool(
        description = "Rise / set times above min_alt_degrees on a given UTC date. \
                       null bounds for circumpolar always-up or always-down. \
                       Requires `site`."
    )]
    pub(crate) async fn compute_rise_set(
        &self,
        Parameters(params): Parameters<RiseSetParams>,
    ) -> Result<CallToolResult, rmcp::ErrorData> {
        let Some(site) = self.site.as_ref() else {
            return Ok(tool_error!(
                "{}",
                crate::planner::primitives::site_required_error()
            ));
        };
        let target = match crate::planner::primitives::validate_icrs(params.ra, params.dec) {
            Ok(c) => c,
            Err(e) => return Ok(tool_error!("{}", e)),
        };
        let date = match crate::planner::primitives::parse_date(&params.date) {
            Ok(d) => d,
            Err(e) => return Ok(tool_error!("{}", e)),
        };
        if !(-90.0..=90.0).contains(&params.min_alt_degrees) {
            return Ok(tool_error!(
                "min_alt_degrees must be in [-90, 90]; got {}",
                params.min_alt_degrees
            ));
        }
        let v = crate::planner::primitives::compute_rise_set(
            site,
            target,
            date,
            params.min_alt_degrees,
        );
        Ok(CallToolResult::success(vec![ContentBlock::text(
            v.to_string(),
        )]))
    }

    #[tool(
        description = "Time-to-flip (seconds) until the target next reaches the meridian \
                       (HA = 0). v1 ignores side_of_pier but accepts it for forward \
                       compatibility. Requires `site`."
    )]
    pub(crate) async fn compute_meridian_flip(
        &self,
        Parameters(params): Parameters<MeridianFlipParams>,
    ) -> Result<CallToolResult, rmcp::ErrorData> {
        let Some(site) = self.site.as_ref() else {
            return Ok(tool_error!(
                "{}",
                crate::planner::primitives::site_required_error()
            ));
        };
        let target = match crate::planner::primitives::validate_icrs(params.ra, params.dec) {
            Ok(c) => c,
            Err(e) => return Ok(tool_error!("{}", e)),
        };
        let time = match crate::planner::primitives::parse_time_or_now(params.time.as_deref()) {
            Ok(t) => t,
            Err(e) => return Ok(tool_error!("{}", e)),
        };
        let side = match crate::planner::primitives::parse_side_of_pier(&params.side_of_pier) {
            Ok(s) => s,
            Err(e) => return Ok(tool_error!("{}", e)),
        };
        let v = crate::planner::primitives::compute_meridian_flip(site, target, time, side);
        Ok(CallToolResult::success(vec![ContentBlock::text(
            v.to_string(),
        )]))
    }

    #[tool(
        description = "Geocentric astrometric Sun position + topocentric alt/az. Requires `site`."
    )]
    pub(crate) async fn get_sun_position(
        &self,
        Parameters(params): Parameters<TimeOnlyParams>,
    ) -> Result<CallToolResult, rmcp::ErrorData> {
        let Some(site) = self.site.as_ref() else {
            return Ok(tool_error!(
                "{}",
                crate::planner::primitives::site_required_error()
            ));
        };
        let time = match crate::planner::primitives::parse_time_or_now(params.time.as_deref()) {
            Ok(t) => t,
            Err(e) => return Ok(tool_error!("{}", e)),
        };
        let v = crate::planner::primitives::get_sun_position(site, time);
        Ok(CallToolResult::success(vec![ContentBlock::text(
            v.to_string(),
        )]))
    }

    #[tool(
        description = "Civil / nautical / astronomical twilight bounds for the local \
                       night that covers `date` (UTC). null bound at high latitudes \
                       where the Sun never crosses the threshold. Requires `site`."
    )]
    pub(crate) async fn get_twilight(
        &self,
        Parameters(params): Parameters<TwilightParams>,
    ) -> Result<CallToolResult, rmcp::ErrorData> {
        let Some(site) = self.site.as_ref() else {
            return Ok(tool_error!(
                "{}",
                crate::planner::primitives::site_required_error()
            ));
        };
        let date = match crate::planner::primitives::parse_date(&params.date) {
            Ok(d) => d,
            Err(e) => return Ok(tool_error!("{}", e)),
        };
        let kind = match crate::planner::primitives::parse_twilight_kind(&params.kind) {
            Ok(k) => k,
            Err(e) => return Ok(tool_error!("{}", e)),
        };
        let v = crate::planner::primitives::get_twilight(site, date, kind);
        Ok(CallToolResult::success(vec![ContentBlock::text(
            v.to_string(),
        )]))
    }

    #[tool(
        description = "Geocentric Moon position + topocentric alt/az + Sun-Moon \
                       elongation (phase) + illuminated fraction. Requires `site`."
    )]
    pub(crate) async fn get_moon_position(
        &self,
        Parameters(params): Parameters<TimeOnlyParams>,
    ) -> Result<CallToolResult, rmcp::ErrorData> {
        let Some(site) = self.site.as_ref() else {
            return Ok(tool_error!(
                "{}",
                crate::planner::primitives::site_required_error()
            ));
        };
        let time = match crate::planner::primitives::parse_time_or_now(params.time.as_deref()) {
            Ok(t) => t,
            Err(e) => return Ok(tool_error!("{}", e)),
        };
        let v = crate::planner::primitives::get_moon_position(site, time);
        Ok(CallToolResult::success(vec![ContentBlock::text(
            v.to_string(),
        )]))
    }

    #[tool(
        description = "Angular separation (degrees) between an ICRS target and the \
                       Moon. Geocentric — does not depend on `site`."
    )]
    pub(crate) async fn compute_moon_separation(
        &self,
        Parameters(params): Parameters<MoonSeparationParams>,
    ) -> Result<CallToolResult, rmcp::ErrorData> {
        let target = match crate::planner::primitives::validate_icrs(params.ra, params.dec) {
            Ok(c) => c,
            Err(e) => return Ok(tool_error!("{}", e)),
        };
        let time = match crate::planner::primitives::parse_time_or_now(params.time.as_deref()) {
            Ok(t) => t,
            Err(e) => return Ok(tool_error!("{}", e)),
        };
        let v = crate::planner::primitives::compute_moon_separation(target, time);
        Ok(CallToolResult::success(vec![ContentBlock::text(
            v.to_string(),
        )]))
    }

    #[tool(description = "The configured observer site: latitude_degrees (north \
                       positive) and longitude_degrees (east positive). \
                       Cross-checked against the mount's reported site on \
                       connect when the mount exposes one. Requires `site`.")]
    pub(crate) async fn get_site(
        &self,
        Parameters(_params): Parameters<GetSiteParams>,
    ) -> Result<CallToolResult, rmcp::ErrorData> {
        let Some(site) = self.site.as_ref() else {
            return Ok(tool_error!(
                "{}",
                crate::planner::primitives::site_required_error()
            ));
        };
        Ok(tool_success!({
            "latitude_degrees": site.latitude_degrees,
            "longitude_degrees": site.longitude_degrees,
        }))
    }

    #[tool(description = "Local apparent sidereal time at the configured site. Requires `site`.")]
    pub(crate) async fn get_local_sidereal_time(
        &self,
        Parameters(params): Parameters<TimeOnlyParams>,
    ) -> Result<CallToolResult, rmcp::ErrorData> {
        let Some(site) = self.site.as_ref() else {
            return Ok(tool_error!(
                "{}",
                crate::planner::primitives::site_required_error()
            ));
        };
        let time = match crate::planner::primitives::parse_time_or_now(params.time.as_deref()) {
            Ok(t) => t,
            Err(e) => return Ok(tool_error!("{}", e)),
        };
        let v = crate::planner::primitives::get_local_sidereal_time(site, time);
        Ok(CallToolResult::success(vec![ContentBlock::text(
            v.to_string(),
        )]))
    }

    // -------------------------------------------------------------------
    // Convenience tools (get_target_status, get_next_target,
    // get_meridian_status) — see docs/services/rp.md §"Dynamic Planner"
    // -------------------------------------------------------------------

    #[tool(description = "Sky position + progress for a target. Accepts either \
                       target_name (resolved via the embedded catalog) or a \
                       raw ra/dec pair. progress is the per-goal list \
                       {filter, binning, exposure_duration, desired_count, \
                       good, total} derived from the frames on disk when \
                       target_name (as given or catalog-resolved) slugifies \
                       to an active target-store row, null otherwise. \
                       Requires `site`.")]
    pub(crate) async fn get_target_status(
        &self,
        Parameters(params): Parameters<GetTargetStatusParams>,
    ) -> Result<CallToolResult, rmcp::ErrorData> {
        let Some(site) = self.site.as_ref() else {
            return Ok(tool_error!(
                "{}",
                crate::planner::primitives::site_required_error()
            ));
        };
        let time = match crate::planner::primitives::parse_time_or_now(params.time.as_deref()) {
            Ok(t) => t,
            Err(e) => return Ok(tool_error!("{}", e)),
        };
        let (target, name) = match (params.target_name.as_ref(), params.ra, params.dec) {
            (Some(name), None, None) => match crate::planner::catalog::resolve(name) {
                crate::planner::catalog::ResolveOutcome::Resolved(view) => (
                    rp_ephemeris::IcrsCoord {
                        ra_hours: view.ra_hours,
                        dec_degrees: view.dec_degrees,
                    },
                    view.name,
                ),
                crate::planner::catalog::ResolveOutcome::NotFound { suggestions } => {
                    return Ok(CallToolResult::error(vec![ContentBlock::text(
                        serde_json::json!({
                            "error": "target_not_found",
                            "name": name,
                            "suggestions": suggestions,
                        })
                        .to_string(),
                    )]));
                }
            },
            (None, Some(ra), Some(dec)) => {
                match crate::planner::primitives::validate_icrs(ra, dec) {
                    Ok(c) => (c, format!("ICRS({ra:.4}, {dec:.4})")),
                    Err(e) => return Ok(tool_error!("{}", e)),
                }
            }
            _ => {
                return Ok(tool_error!(
                    "supply exactly one of `target_name` or (`ra` + `dec`)"
                ))
            }
        };
        // Progress is keyed by a target's slug (its stable identity),
        // while `get_target_status` is queried by a free-form or catalog
        // name — so slugify both the caller's `target_name` and its
        // catalog-resolved form (`name`) and match those against the
        // active store rows. The ra/dec form has no name to match and
        // reports progress: null.
        let progress = match params.target_name.as_deref() {
            Some(raw) => {
                // Both spellings, one derivation: the caller's
                // `target_name` and its catalog-resolved form go through
                // the same `from_display_name` every writer used, so the
                // slug matched here is by construction the slug stored.
                let wanted: Vec<String> = [raw, name.as_str()]
                    .into_iter()
                    .filter_map(|s| rp_targets::TargetSlug::from_display_name(s).ok())
                    .map(|slug| slug.as_str().to_string())
                    .collect();
                let targets = self.active_targets().await;
                match targets
                    .iter()
                    .find(|t| wanted.iter().any(|w| w == t.slug.as_str()))
                {
                    Some(matched) => {
                        let counts = self.derive_progress(matched).await;
                        serde_json::to_value(crate::mcp::built_in::targets::progress_rows(
                            matched, &counts,
                        ))
                        .unwrap_or(serde_json::Value::Null)
                    }
                    None => serde_json::Value::Null,
                }
            }
            None => serde_json::Value::Null,
        };
        match crate::planner::convenience::target_status_view(
            site,
            target,
            &name,
            time,
            self.default_min_altitude_degrees,
            progress,
        ) {
            Ok(v) => Ok(CallToolResult::success(vec![ContentBlock::text(
                v.to_string(),
            )])),
            Err(e) => Ok(tool_error!("{}", e)),
        }
    }

    #[tool(description = "Recommend the next target from the active rows of the \
                       target store (rp.md § Target Store), based on altitude / \
                       approaching transit / integration progress / \
                       sun-elevation gating. A store-backed target's altitude \
                       floor is target.scheduling.min_altitude_degrees, \
                       falling back to target_store.default_scheduling.\
                       min_altitude_degrees from config (Decision 9 — \
                       altitude-gating parity, docs/plans/\
                       planetarium-target-import.md). filter and \
                       duration_secs are the recommended target's first \
                       incomplete goal per the record_exposure \
                       counters (null when it has no goals) — a store-backed \
                       target's goals always carry a finite desired_count, so \
                       none of its entries can recommend forever. Returns \
                       target=null and a structured reason \
                       (no_targets_configured / all_below_min_altitude / \
                       wait_for_twilight / end_of_session) when no candidate is \
                       viable: wait_for_twilight = the Sun is brighter than \
                       astronomical dusk and not rising (evening — wait and \
                       re-ask); end_of_session = brighter and rising (dawn) or \
                       every target's integration goal is met — the session is \
                       over. position_angle_degrees is the recommended \
                       target's effective framing angle (its own value, else \
                       the train_id train's default_position_angle_degrees, \
                       else 0.0 north-up; null when target is null) — pass \
                       train_id (the imaging train) so the per-train layer \
                       applies and so the filter-batching tie-break reads \
                       that train's filter wheel (without it, the rig's only \
                       wheel; no readable wheel leaves the tie-break \
                       neutral). Requires `site`.")]
    pub(crate) async fn get_next_target(
        &self,
        Parameters(params): Parameters<GetNextTargetParams>,
    ) -> Result<CallToolResult, rmcp::ErrorData> {
        let Some(site) = self.site.as_ref() else {
            return Ok(tool_error!(
                "{}",
                crate::planner::primitives::site_required_error()
            ));
        };
        let time = match crate::planner::primitives::parse_time_or_now(params.time.as_deref()) {
            Ok(t) => t,
            Err(e) => return Ok(tool_error!("{}", e)),
        };
        // The named imaging train supplies layer two of the effective
        // position angle (rp.md § Target Store → Position angle). An
        // unknown id is a caller bug — fail loudly rather than
        // silently recommending north-up framing all night.
        let train_default_position_angle_deg = match params.train_id.as_deref() {
            None => None,
            Some(id) => match self.trains.train(id) {
                Some(train) => train.default_position_angle_degrees,
                None => {
                    return Ok(tool_error!(
                        "unknown train_id `{}`: not an equipment.optical_trains[] id",
                        id
                    ))
                }
            },
        };
        let eph = rp_ephemeris::ErfarsEphemeris::new();
        // Candidates are every active store row (Decision 9), projected
        // onto the decision candidate type, paired with the progress
        // derived from their frames on disk and the filter in the wheel
        // — both read here, at the tool boundary, so the decision stays
        // pure.
        let (candidates, progress) = self.planner_snapshot().await;
        let current_filter = self
            .current_filter_for_planner(params.train_id.as_deref())
            .await;
        // A store-backed target's own default floor, falling back to
        // the planner-wide default (`planner.min_altitude_degrees`).
        let default_min_altitude_degrees = self
            .target_store_defaults
            .default_scheduling
            .min_altitude_degrees
            .unwrap_or(self.default_min_altitude_degrees);
        let rec = crate::planner::decision::next_target(
            &eph,
            site,
            time,
            &candidates,
            default_min_altitude_degrees,
            train_default_position_angle_deg,
            &progress,
            current_filter.as_deref(),
        );
        // The recommendation's derived `Serialize` is the wire contract
        // (see `PlannerTarget`): `target` nests its `coord`, and the
        // selected plan entry surfaces as a nested `exposure` object.
        let v = serde_json::to_value(&rec).unwrap_or(serde_json::Value::Null);
        Ok(CallToolResult::success(vec![ContentBlock::text(
            v.to_string(),
        )]))
    }

    #[tool(
        description = "Read back an active target-store row's progress after a \
                       frame. It increments nothing — capture already wrote \
                       the frame the scan finds — and records nothing (the \
                       planner's filter-batching tie-break reads the wheel). \
                       Returns {target, filter, progress}, where filter is the \
                       argument normalised (null for the unfiltered slot — an \
                       omitted, null or \"\" filter — else the name as given) \
                       and progress is the per-goal list {filter, binning, \
                       exposure_duration, desired_count, good, total} derived \
                       from the frames on disk."
    )]
    pub(crate) async fn record_exposure(
        &self,
        Parameters(params): Parameters<RecordExposureParams>,
    ) -> Result<CallToolResult, rmcp::ErrorData> {
        // Recording against an unknown slug means the orchestrator
        // believes it is imaging a target the planner cannot see — a
        // typo'd call should fail loudly rather than silently succeed.
        let targets = self.active_targets().await;
        let Some(target) = targets.iter().find(|t| t.slug.as_str() == params.target) else {
            return Ok(tool_error!(
                "unknown target `{}`: not an active target-store row",
                params.target
            ));
        };
        // Nothing is stored: frame counts live in the frames, and the
        // filter tie-break reads the wheel. The filter is echoed back.
        let key = crate::planner::progress::filter_key(params.filter.as_deref());
        let counts = self.derive_progress(target).await;
        Ok(tool_success!({
            "target": target.slug.as_str(),
            "filter": if key.is_empty() { serde_json::Value::Null } else { serde_json::Value::String(key) },
            "progress": crate::mcp::built_in::targets::progress_rows(target, &counts),
        }))
    }

    #[tool(
        description = "Full progress overview derived from the frames on disk: \
                       target slug -> the per-goal list {filter, binning, \
                       exposure_duration, desired_count, good, total}, for \
                       every active target-store row."
    )]
    pub(crate) async fn get_session_progress(
        &self,
        Parameters(_params): Parameters<GetSessionProgressParams>,
    ) -> Result<CallToolResult, rmcp::ErrorData> {
        let targets = self.active_targets().await;
        // BTreeMap so the payload's target order is stable across calls.
        let mut progress = std::collections::BTreeMap::new();
        for target in &targets {
            let counts = self.derive_progress(target).await;
            progress.insert(
                target.slug.as_str().to_string(),
                crate::mcp::built_in::targets::progress_rows(target, &counts),
            );
        }
        Ok(tool_success!({ "progress": progress }))
    }

    #[tool(
        description = "Time-to-flip plus side-of-pier for the mount's current \
                       pointing. Reads RA/Dec/SideOfPier from the configured \
                       mount, then runs the meridian-flip primitive. Requires \
                       `site` and a connected mount."
    )]
    pub(crate) async fn get_meridian_status(
        &self,
        Parameters(params): Parameters<GetMeridianStatusParams>,
    ) -> Result<CallToolResult, rmcp::ErrorData> {
        let Some(site) = self.site.as_ref() else {
            return Ok(tool_error!(
                "{}",
                crate::planner::primitives::site_required_error()
            ));
        };
        let time = match crate::planner::primitives::parse_time_or_now(params.time.as_deref()) {
            Ok(t) => t,
            Err(e) => return Ok(tool_error!("{}", e)),
        };
        let (_entry, mount) = match self.resolve_mount() {
            Ok(p) => p,
            Err(e) => return Ok(tool_error!("{}", e)),
        };
        let ra = match mount.right_ascension().await {
            Ok(v) => v,
            Err(e) => return Ok(tool_error!("failed to read mount right_ascension: {}", e)),
        };
        let dec = match mount.declination().await {
            Ok(v) => v,
            Err(e) => return Ok(tool_error!("failed to read mount declination: {}", e)),
        };
        // ASCOM SideOfPier returns `NOT_IMPLEMENTED` on mounts that
        // don't expose the property — treat that specifically as
        // `Unknown` so the flip ETA still surfaces. Any other read
        // failure (network error, transient Alpaca issue, etc.) is
        // surfaced loudly: a "valid-looking but stale" payload is
        // worse than a clean error the operator can act on.
        let side = match mount.side_of_pier().await {
            Ok(ascom_alpaca::api::telescope::PierSide::East) => rp_ephemeris::SideOfPier::East,
            Ok(ascom_alpaca::api::telescope::PierSide::West) => rp_ephemeris::SideOfPier::West,
            Ok(_) => rp_ephemeris::SideOfPier::Unknown,
            Err(e) if e.code == ascom_alpaca::ASCOMErrorCode::NOT_IMPLEMENTED => {
                rp_ephemeris::SideOfPier::Unknown
            }
            Err(e) => return Ok(tool_error!("failed to read mount side_of_pier: {}", e)),
        };
        let v = crate::planner::convenience::meridian_status_view(site, ra, dec, time, side);
        Ok(CallToolResult::success(vec![ContentBlock::text(
            v.to_string(),
        )]))
    }
}
