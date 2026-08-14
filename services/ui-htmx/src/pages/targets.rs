//! The targets inbox (`/targets`) — rp's target store as an operator
//! surface (`docs/services/ui-htmx.md` "Targets inbox"): pending targets
//! await review, active targets sit in the roster section, and the
//! per-target review page edits display name, priority, notes, the
//! framing position angle, and the acquisition goals, then activates or
//! discards the target.
//!
//! Every read and write rides rp's MCP-only target tools through the
//! [`crate::targets_client`] seam; the filter roster and train
//! position-angle defaults join in from rp's config over REST. The htmx
//! swap unit is `#targets-page` — the inbox and the review form are
//! alternate states of that one element.

use std::collections::BTreeSet;

use axum::extract::{Form, Path, Query, State};
use axum::http::{HeaderMap, HeaderValue};
use axum::response::{IntoResponse, Response};
use maud::{html, Markup};
use serde_json::{json, Map, Value};

use crate::pages::{layout_with_nav, NavTab};
use crate::targets_client::TargetsError;
use crate::{AppState, RpState};

/// Page title for the targets routes.
const TITLE: &str = "rusty-photon · targets";

/// The stale-goal badge text — a goal whose filter the rig's roster no
/// longer carries would fail mid-session at capture time.
const GOAL_FLAG: &str = "not in the rig's filter roster";

fn is_htmx(headers: &HeaderMap) -> bool {
    headers.contains_key("HX-Request")
}

/// Wrap a `#targets-page` fragment in the full page unless htmx asked.
fn respond(fragment: Markup, headers: &HeaderMap) -> Response {
    if is_htmx(headers) {
        fragment.into_response()
    } else {
        layout_with_nav(TITLE, NavTab::Targets, fragment).into_response()
    }
}

/// Mutation responses push the inbox URL so a browser refresh after an
/// activate/discard lands on `/targets` (the equipment page's pattern).
fn with_push_url(mut response: Response) -> Response {
    response
        .headers_mut()
        .insert("HX-Push-Url", HeaderValue::from_static("/targets"));
    response
}

// --- the data joins --------------------------------------------------------

/// One acquisition goal, as the tool wire shape renders it.
#[derive(Debug, Clone, Default)]
struct Goal {
    filter: String,
    binning: String,
    exposure: String,
    desired_count: String,
}

/// One store row, parsed leniently from `list_targets` / `get_target`
/// output (`Target`'s derived serialization — rp.md § Target MCP tools).
#[derive(Debug, Clone, Default)]
struct Row {
    slug: String,
    display_name: String,
    active: bool,
    ra_hours: f64,
    dec_degrees: f64,
    catalog_ref: Option<String>,
    object_type: Option<String>,
    magnitude: Option<f64>,
    position_angle_degrees: Option<f64>,
    priority: i64,
    goals: Vec<Goal>,
    notes: Option<String>,
    created_by: String,
    updated_by: String,
    updated_at: String,
}

fn parse_row(value: &Value) -> Row {
    let text = |field: &str| {
        value
            .get(field)
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string()
    };
    let opt_text = |field: &str| value.get(field).and_then(Value::as_str).map(str::to_string);
    Row {
        slug: text("slug"),
        display_name: text("display_name"),
        active: value
            .get("active")
            .and_then(Value::as_bool)
            .unwrap_or(false),
        ra_hours: value
            .pointer("/coord/ra_hours")
            .and_then(Value::as_f64)
            .unwrap_or_default(),
        dec_degrees: value
            .pointer("/coord/dec_degrees")
            .and_then(Value::as_f64)
            .unwrap_or_default(),
        catalog_ref: opt_text("catalog_ref"),
        object_type: opt_text("object_type"),
        magnitude: value.get("magnitude").and_then(Value::as_f64),
        position_angle_degrees: value.get("position_angle_degrees").and_then(Value::as_f64),
        priority: value.get("priority").and_then(Value::as_i64).unwrap_or(0),
        goals: value
            .get("goals")
            .and_then(Value::as_array)
            .map(|goals| {
                goals
                    .iter()
                    .map(|g| Goal {
                        filter: g
                            .get("filter")
                            .and_then(Value::as_str)
                            .unwrap_or_default()
                            .to_string(),
                        binning: g
                            .get("binning")
                            .and_then(Value::as_str)
                            .unwrap_or_default()
                            .to_string(),
                        exposure: g
                            .get("exposure_duration")
                            .and_then(Value::as_str)
                            .unwrap_or_default()
                            .to_string(),
                        desired_count: g
                            .get("desired_count")
                            .and_then(Value::as_u64)
                            .unwrap_or_default()
                            .to_string(),
                    })
                    .collect()
            })
            .unwrap_or_default(),
        notes: opt_text("notes"),
        created_by: text("created_by"),
        updated_by: text("updated_by"),
        updated_at: text("updated_at"),
    }
}

/// What the targets pages join in from rp's effective config: the filter
/// roster union goals are validated against, and the imaging trains'
/// configured default position angles (the PA field's inherit hint).
#[derive(Debug, Default)]
struct ConfigJoin {
    filter_roster: BTreeSet<String>,
    train_defaults: Vec<(String, f64)>,
}

fn config_join(config: &Value) -> ConfigJoin {
    let mut join = ConfigJoin::default();
    if let Some(wheels) = config
        .pointer("/equipment/filter_wheels")
        .and_then(Value::as_array)
    {
        for wheel in wheels {
            if let Some(filters) = wheel.get("filters").and_then(Value::as_array) {
                join.filter_roster
                    .extend(filters.iter().filter_map(Value::as_str).map(str::to_string));
            }
        }
    }
    if let Some(trains) = config
        .pointer("/equipment/optical_trains")
        .and_then(Value::as_array)
    {
        for train in trains {
            let imaging = train
                .get("purpose")
                .and_then(Value::as_str)
                .is_none_or(|p| p == "imaging");
            let id = train.get("id").and_then(Value::as_str).unwrap_or_default();
            if let Some(angle) = train
                .get("default_position_angle_degrees")
                .and_then(Value::as_f64)
            {
                if imaging && !id.is_empty() {
                    join.train_defaults.push((id.to_string(), angle));
                }
            }
        }
    }
    join
}

/// Whether a goal's filter should carry the stale badge: the roster union
/// is non-empty (rp's own permissive rule — no wheels, no validation) and
/// does not contain the filter.
fn goal_flagged(filter: &str, roster: &BTreeSet<String>) -> bool {
    !filter.is_empty() && !roster.is_empty() && !roster.contains(filter)
}

/// Pending first-to-review ordering: newest `updated_at` first (RFC 3339
/// sorts lexicographically). Active rows read as a roster: by name.
fn sort_rows(rows: &mut [Row]) {
    rows.sort_by(|a, b| {
        if a.active == b.active {
            if a.active {
                a.display_name.cmp(&b.display_name)
            } else {
                b.updated_at.cmp(&a.updated_at)
            }
        } else {
            a.active.cmp(&b.active)
        }
    });
}

// --- error cards -----------------------------------------------------------

/// The honest one-card unavailable state: rp down and the `/mcp` safety
/// gate read identically from out here, so both causes are named.
fn unavailable_card(detail: &str) -> Markup {
    html! {
        div #targets-page.card {
            div class="banner error targets-unavailable" {
                span.dot {}
                span {
                    "rp's target tools are unavailable: " (detail)
                    ". rp may be down, or its MCP surface is safety-gated \
                     (503) while conditions are unsafe."
                }
            }
            p {
                button.link type="button" hx-get="/targets"
                    hx-target="#targets-page" hx-swap="outerHTML" { "Retry" }
            }
        }
    }
}

fn error_card(message: &str) -> Markup {
    html! {
        div #targets-page.card {
            div class="banner error" { span.dot {} span { (message) } }
            p {
                button.link type="button" hx-get="/targets"
                    hx-target="#targets-page" hx-swap="outerHTML" { "Back to targets" }
            }
        }
    }
}

fn targets_error_card(err: &TargetsError) -> Markup {
    match err {
        TargetsError::Unavailable(detail) => unavailable_card(detail),
        TargetsError::Tool(msg) | TargetsError::Malformed(msg) => error_card(msg),
    }
}

// --- the inbox state -------------------------------------------------------

/// Fetch the store and the config join; render the two-section listing.
async fn inbox_state(rp: &RpState) -> Markup {
    let targets = match rp.targets.list_targets().await {
        Ok(targets) => targets,
        Err(err) => return targets_error_card(&err),
    };
    // One rp, one health state: the config join failing renders the same
    // unavailable card rather than a page with silently absent flags.
    let config = match rp.config_client.get_config().await {
        Ok(resp) => resp.config,
        Err(err) => return unavailable_card(&err.to_string()),
    };
    let join = config_join(&config);
    let mut rows: Vec<Row> = targets.iter().map(parse_row).collect();
    sort_rows(&mut rows);
    inbox_markup(&rows, &join)
}

fn inbox_markup(rows: &[Row], join: &ConfigJoin) -> Markup {
    let pending: Vec<&Row> = rows.iter().filter(|r| !r.active).collect();
    let active: Vec<&Row> = rows.iter().filter(|r| r.active).collect();
    html! {
        div #targets-page.card {
            @if rows.is_empty() {
                div.empty-state {
                    h2 { "No targets yet" }
                    p {
                        "Press Align in your planetarium to import a framed \
                         target through the planetarium bridge, or add one \
                         with rp's add_target tool over MCP. Imports land \
                         here, paused, for review."
                    }
                }
            } @else {
                section #inbox {
                    h2 { "Inbox" }
                    @if pending.is_empty() {
                        p.empty-note { "Inbox empty — every target is active." }
                    }
                    @for row in &pending {
                        (row_markup(row, join))
                    }
                }
                section #active-roster {
                    h2 { "Active roster" }
                    @if active.is_empty() {
                        p.empty-note { "No active targets yet." }
                    }
                    @for row in &active {
                        (row_markup(row, join))
                    }
                }
            }
        }
    }
}

fn row_markup(row: &Row, join: &ConfigJoin) -> Markup {
    html! {
        div.target-row data-slug=(row.slug) {
            div.row-head {
                span.name { (row.display_name) }
                span.slug { (row.slug) }
            }
            div.coords {
                (format!("RA {:.4} h · Dec {:+.3}°", row.ra_hours, row.dec_degrees))
                @if let Some(catalog_ref) = &row.catalog_ref {
                    " · " (catalog_ref)
                }
                @if let Some(object_type) = &row.object_type {
                    " · " (object_type)
                }
                @if let Some(magnitude) = row.magnitude {
                    (format!(" · mag {magnitude}"))
                }
            }
            div.position-angle {
                "Position angle: "
                @match row.position_angle_degrees {
                    Some(angle) => (format!("{angle}°")),
                    None => "inherit",
                }
            }
            div.goals {
                @if row.goals.is_empty() {
                    span.goal.none { "No acquisition goals yet" }
                }
                @for goal in &row.goals {
                    span.goal {
                        (format!("{} × {} {} {}", goal.desired_count, goal.exposure, goal.filter, goal.binning))
                        @if goal_flagged(&goal.filter, &join.filter_roster) {
                            span.goal-flag { (format!("{:?} {GOAL_FLAG}", goal.filter)) }
                        }
                    }
                }
            }
            (provenance_markup(row))
            div.actions {
                a.review href={ "/targets/" (row.slug) } { "Review" }
                @if row.active {
                    button.pause type="button"
                        hx-post={ "/targets/" (row.slug) "/active?active=false" }
                        hx-target="#targets-page" hx-swap="outerHTML" { "Pause" }
                } @else {
                    button.activate type="button"
                        hx-post={ "/targets/" (row.slug) "/active?active=true" }
                        hx-target="#targets-page" hx-swap="outerHTML" { "Activate" }
                    button.discard type="button"
                        hx-post={ "/targets/" (row.slug) "/delete" }
                        hx-target="#targets-page" hx-swap="outerHTML"
                        hx-confirm={ "Discard " (row.display_name)
                            "? The store row is removed; frames on disk are untouched." }
                        { "Discard" }
                }
            }
        }
    }
}

fn provenance_markup(row: &Row) -> Markup {
    html! {
        div.provenance {
            span {
                "added by " (row.created_by)
                " · last touched by " (row.updated_by)
                " at " (row.updated_at)
            }
            @if let Some(notes) = &row.notes {
                div.notes { (notes) }
            }
        }
    }
}

// --- the review form -------------------------------------------------------

/// The editable values the form renders — either the stored row's (on GET
/// and after a successful save) or the operator's submitted ones (on a
/// rejected save, so nothing typed is lost).
#[derive(Debug, Default)]
struct FormEcho {
    display_name: String,
    priority: String,
    /// The raw field text: explicit angle, or empty = inherit.
    position_angle: String,
    goals: Vec<Goal>,
}

impl FormEcho {
    fn from_row(row: &Row) -> Self {
        Self {
            display_name: row.display_name.clone(),
            priority: row.priority.to_string(),
            position_angle: row
                .position_angle_degrees
                .map(trim_float)
                .unwrap_or_default(),
            goals: row.goals.clone(),
        }
    }
}

/// `254.0` renders as `254`, `121.25` as `121.25` — the field holds what
/// an operator would type, not float formatting noise.
fn trim_float(value: f64) -> String {
    let text = format!("{value}");
    text.strip_suffix(".0").map_or(text.clone(), str::to_string)
}

/// Field-level errors on the review form (`div.field.invalid` + `.error`,
/// the config pages' convention).
#[derive(Debug, Default)]
struct FormErrors {
    display_name: Option<String>,
    priority: Option<String>,
    position_angle: Option<String>,
}

impl FormErrors {
    const fn any(&self) -> bool {
        self.display_name.is_some() || self.priority.is_some() || self.position_angle.is_some()
    }
}

/// The banner over the review form.
enum ReviewBanner {
    Saved,
    /// `update_target` succeeded but `set_goals` was rejected — the store
    /// has no cross-call transaction, so say so honestly.
    GoalsRejected(String),
    Problem(String),
}

fn review_markup(
    row: &Row,
    echo: &FormEcho,
    errors: &FormErrors,
    banner: Option<&ReviewBanner>,
    join: &ConfigJoin,
) -> Markup {
    html! {
        div #targets-page.card {
            @if let Some(banner) = banner {
                @match banner {
                    ReviewBanner::Saved => div class="banner ok" { span.dot {} span { "Saved." } },
                    ReviewBanner::GoalsRejected(msg) => div class="banner error" {
                        span.dot {} span { "Details saved; goals were not: " (msg) }
                    },
                    ReviewBanner::Problem(msg) => div class="banner error" {
                        span.dot {} span { (msg) }
                    },
                }
            }
            h2 { (row.display_name) }
            div.meta {
                span.slug { (row.slug) }
                div.coords {
                    (format!("RA {:.4} h · Dec {:+.3}°", row.ra_hours, row.dec_degrees))
                    @if let Some(catalog_ref) = &row.catalog_ref { " · " (catalog_ref) }
                }
                (provenance_markup(row))
            }
            form #target-form hx-post={ "/targets/" (row.slug) }
                hx-target="#targets-page" hx-swap="outerHTML" {
                (text_field("Display name", "display_name", &echo.display_name,
                    errors.display_name.as_deref()))
                (text_field("Priority", "priority", &echo.priority,
                    errors.priority.as_deref()))
                div class={ "field" @if errors.position_angle.is_some() { " invalid" } } {
                    label for="position_angle_degrees" { "Position angle" }
                    input type="text" name="position_angle_degrees"
                        id="position_angle_degrees" value=(echo.position_angle);
                    div.pa-hint { (pa_hint(join)) }
                    @if let Some(error) = &errors.position_angle {
                        span.error { (error) }
                    }
                }
                div.field {
                    label for="notes" { "Notes" }
                    textarea name="notes" id="notes" { (row.notes.clone().unwrap_or_default()) }
                }
                fieldset {
                    legend { "Acquisition goals" }
                    div #goal-rows {
                        @for goal in &echo.goals {
                            (goal_row_markup(goal, join))
                        }
                    }
                    button.add-goal type="button" hx-get="/targets/goal-row"
                        hx-target="#goal-rows" hx-swap="beforeend" { "Add goal" }
                }
                button type="submit" { "Save" }
            }
            div.actions {
                @if row.active {
                    button.pause type="button"
                        hx-post={ "/targets/" (row.slug) "/active?active=false" }
                        hx-target="#targets-page" hx-swap="outerHTML" { "Pause" }
                } @else {
                    button.activate type="button"
                        hx-post={ "/targets/" (row.slug) "/active?active=true" }
                        hx-target="#targets-page" hx-swap="outerHTML" { "Activate" }
                }
                button.discard type="button"
                    hx-post={ "/targets/" (row.slug) "/delete" }
                    hx-target="#targets-page" hx-swap="outerHTML"
                    hx-confirm={ "Delete " (row.display_name) "?" } { "Delete" }
                p.help {
                    "Deleting removes the plan row only; frames on disk are \
                     left orphaned — prefer Pause for a target that has \
                     captured frames."
                }
            }
        }
    }
}

fn text_field(label: &str, name: &str, value: &str, error: Option<&str>) -> Markup {
    html! {
        div class={ "field" @if error.is_some() { " invalid" } } {
            label for=(name) { (label) }
            input type="text" name=(name) id=(name) value=(value);
            @if let Some(error) = error {
                span.error { (error) }
            }
        }
    }
}

/// What "blank" currently means, from the config join: the imaging
/// trains' configured defaults, else north-up.
fn pa_hint(join: &ConfigJoin) -> String {
    if join.train_defaults.is_empty() {
        "blank = inherit — north-up (0°)".to_string()
    } else {
        let defaults: Vec<String> = join
            .train_defaults
            .iter()
            .map(|(id, angle)| format!("{id}: {angle:.1}°"))
            .collect();
        format!("blank = inherit — {}", defaults.join(", "))
    }
}

fn goal_row_markup(goal: &Goal, join: &ConfigJoin) -> Markup {
    html! {
        div.goal-row {
            input type="text" name="goal_filter" value=(goal.filter)
                placeholder="filter" list="filter-roster";
            input type="text" name="goal_binning" value=(goal.binning)
                placeholder="1x1";
            input type="text" name="goal_exposure" value=(goal.exposure)
                placeholder="5m";
            input type="text" name="goal_count" value=(goal.desired_count)
                placeholder="frames";
            @if goal_flagged(&goal.filter, &join.filter_roster) {
                span.goal-flag { (GOAL_FLAG) }
            }
            button.remove-goal type="button" hx-get="/targets/goal-row?remove=1"
                hx-target="closest .goal-row" hx-swap="outerHTML" { "Remove" }
            datalist id="filter-roster" {
                @for filter in &join.filter_roster {
                    option value=(filter) {}
                }
            }
        }
    }
}

// --- form parsing ----------------------------------------------------------

/// The position-angle tri-state (the plan's P4 note): blank submits an
/// explicit JSON `null` (clear back to inherit), a parseable number
/// submits that number — so `"0"` is an explicit north-up, never
/// collapsed with blank. Only numeric-ness is checked here; the domain
/// rule (`[0, 360)`, finite) is rp's, surfaced from its own error.
fn parse_position_angle(raw: &str) -> Result<Value, String> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return Ok(Value::Null);
    }
    trimmed
        .parse::<f64>()
        .ok()
        .and_then(serde_json::Number::from_f64)
        .map(Value::Number)
        .ok_or_else(|| {
            "not a number — degrees east of north, or blank to inherit the train default"
                .to_string()
        })
}

/// Zip the goal-row columns back into goal objects, in row order. A row
/// whose every field is empty is dropped (the belt-and-braces companion
/// to the htmx row Remove). Strings pass through untouched — binning and
/// exposure validation is rp's (`GoalWire`'s per-field errors).
fn parse_goal_rows(pairs: &[(String, String)]) -> Result<Vec<Value>, String> {
    let column = |name: &str| -> Vec<&str> {
        pairs
            .iter()
            .filter(|(n, _)| n == name)
            .map(|(_, v)| v.as_str())
            .collect()
    };
    let filters = column("goal_filter");
    let binnings = column("goal_binning");
    let exposures = column("goal_exposure");
    let counts = column("goal_count");
    let rows = filters
        .len()
        .max(binnings.len())
        .max(exposures.len())
        .max(counts.len());
    let cell =
        |col: &Vec<&str>, index: usize| col.get(index).copied().unwrap_or("").trim().to_string();
    let mut goals = Vec::new();
    for index in 0..rows {
        let filter = cell(&filters, index);
        let binning = cell(&binnings, index);
        let exposure = cell(&exposures, index);
        let count = cell(&counts, index);
        if filter.is_empty() && binning.is_empty() && exposure.is_empty() && count.is_empty() {
            continue;
        }
        let desired_count: u32 = count.parse().map_err(|_| {
            format!(
                "goal {}: frame count {count:?} is not a whole number",
                index.saturating_add(1)
            )
        })?;
        goals.push(json!({
            "filter": filter,
            "binning": binning,
            "exposure_duration": exposure,
            "desired_count": desired_count,
        }));
    }
    Ok(goals)
}

fn form_value<'a>(pairs: &'a [(String, String)], name: &str) -> Option<&'a str> {
    pairs
        .iter()
        .rev()
        .find(|(n, _)| n == name)
        .map(|(_, v)| v.as_str())
}

// --- handlers --------------------------------------------------------------

/// `GET /targets` — the inbox.
pub(crate) async fn page(State(state): State<AppState>, headers: HeaderMap) -> Response {
    let Some(rp) = state.rp() else {
        return respond(super::equipment::no_rp_card("the targets inbox"), &headers);
    };
    respond(inbox_state(rp).await, &headers)
}

/// `GET /targets/{slug}` — the review form.
pub(crate) async fn review(
    State(state): State<AppState>,
    Path(slug): Path<String>,
    headers: HeaderMap,
) -> Response {
    let Some(rp) = state.rp() else {
        return respond(super::equipment::no_rp_card("the targets inbox"), &headers);
    };
    respond(review_state(rp, &slug, None, None).await, &headers)
}

/// Fetch one row + the config join and render the form. `echo`/`banner`
/// carry a rejected submission's values and outcome.
async fn review_state(
    rp: &RpState,
    slug: &str,
    override_form: Option<(FormEcho, FormErrors)>,
    banner: Option<ReviewBanner>,
) -> Markup {
    let target = match rp.targets.get_target(slug).await {
        Ok(target) => target,
        Err(TargetsError::Tool(msg)) => {
            return error_card(&format!("no target {slug:?} — {msg}"));
        }
        Err(err) => return targets_error_card(&err),
    };
    let config = match rp.config_client.get_config().await {
        Ok(resp) => resp.config,
        Err(err) => return unavailable_card(&err.to_string()),
    };
    let join = config_join(&config);
    let row = parse_row(&target);
    let (echo, errors) =
        override_form.unwrap_or_else(|| (FormEcho::from_row(&row), FormErrors::default()));
    review_markup(&row, &echo, &errors, banner.as_ref(), &join)
}

/// `POST /targets/{slug}` — save the review form: `update_target` (the
/// scalar subset, with the PA tri-state), then `set_goals` (atomic
/// replacement). Rejections re-render field-level with everything the
/// operator typed preserved.
pub(crate) async fn save(
    State(state): State<AppState>,
    Path(slug): Path<String>,
    headers: HeaderMap,
    Form(form): Form<Vec<(String, String)>>,
) -> Response {
    let Some(rp) = state.rp() else {
        return respond(super::equipment::no_rp_card("the targets inbox"), &headers);
    };

    let display_name = form_value(&form, "display_name")
        .unwrap_or("")
        .trim()
        .to_string();
    let priority_raw = form_value(&form, "priority")
        .unwrap_or("0")
        .trim()
        .to_string();
    let angle_raw = form_value(&form, "position_angle_degrees")
        .unwrap_or("")
        .to_string();
    let notes = form_value(&form, "notes").map(str::to_string);

    let mut echo = FormEcho {
        display_name: display_name.clone(),
        priority: priority_raw.clone(),
        position_angle: angle_raw.clone(),
        goals: Vec::new(),
    };
    let mut errors = FormErrors::default();

    if display_name.is_empty() {
        errors.display_name = Some("a display name is required".to_string());
    }
    let priority: i64 = if let Ok(priority) = priority_raw.parse() {
        priority
    } else {
        errors.priority = Some("not a whole number".to_string());
        0
    };
    let angle = match parse_position_angle(&angle_raw) {
        Ok(angle) => Some(angle),
        Err(message) => {
            errors.position_angle = Some(message);
            None
        }
    };
    let goals = match parse_goal_rows(&form) {
        Ok(goals) => {
            echo.goals = goals
                .iter()
                .map(|g| Goal {
                    filter: g["filter"].as_str().unwrap_or_default().to_string(),
                    binning: g["binning"].as_str().unwrap_or_default().to_string(),
                    exposure: g["exposure_duration"]
                        .as_str()
                        .unwrap_or_default()
                        .to_string(),
                    desired_count: g["desired_count"].as_u64().unwrap_or_default().to_string(),
                })
                .collect();
            Some(goals)
        }
        Err(message) => {
            echo.goals = raw_goal_rows(&form);
            return respond(
                review_state(
                    rp,
                    &slug,
                    Some((echo, errors)),
                    Some(ReviewBanner::Problem(message)),
                )
                .await,
                &headers,
            );
        }
    };

    if errors.any() {
        echo.goals = raw_goal_rows(&form);
        return respond(
            review_state(rp, &slug, Some((echo, errors)), None).await,
            &headers,
        );
    }

    let mut fields = Map::new();
    fields.insert("display_name".to_string(), Value::String(display_name));
    fields.insert("priority".to_string(), json!(priority));
    fields.insert(
        "position_angle_degrees".to_string(),
        angle.clone().unwrap_or(Value::Null),
    );
    if let Some(notes) = notes {
        fields.insert("notes".to_string(), Value::String(notes));
    }
    if let Err(err) = rp.targets.update_target(&slug, fields).await {
        let markup = match err {
            TargetsError::Tool(msg) if msg.contains("position_angle_degrees") => {
                errors.position_angle = Some(msg);
                review_state(rp, &slug, Some((echo, errors)), None).await
            }
            TargetsError::Tool(msg) => {
                review_state(
                    rp,
                    &slug,
                    Some((echo, errors)),
                    Some(ReviewBanner::Problem(msg)),
                )
                .await
            }
            other => targets_error_card(&other),
        };
        return respond(markup, &headers);
    }

    if let Some(goals) = goals {
        if let Err(err) = rp.targets.set_goals(&slug, goals).await {
            let markup = match err {
                TargetsError::Tool(msg) => {
                    review_state(
                        rp,
                        &slug,
                        Some((echo, errors)),
                        Some(ReviewBanner::GoalsRejected(msg)),
                    )
                    .await
                }
                other => targets_error_card(&other),
            };
            return respond(markup, &headers);
        }
    }

    respond(
        review_state(rp, &slug, None, Some(ReviewBanner::Saved)).await,
        &headers,
    )
}

/// The submitted goal rows verbatim (for the error echo, where a row may
/// not parse into a goal object).
fn raw_goal_rows(pairs: &[(String, String)]) -> Vec<Goal> {
    let column = |name: &str| -> Vec<String> {
        pairs
            .iter()
            .filter(|(n, _)| n == name)
            .map(|(_, v)| v.clone())
            .collect()
    };
    let filters = column("goal_filter");
    let binnings = column("goal_binning");
    let exposures = column("goal_exposure");
    let counts = column("goal_count");
    let rows = filters
        .len()
        .max(binnings.len())
        .max(exposures.len())
        .max(counts.len());
    (0..rows)
        .map(|i| Goal {
            filter: filters.get(i).cloned().unwrap_or_default(),
            binning: binnings.get(i).cloned().unwrap_or_default(),
            exposure: exposures.get(i).cloned().unwrap_or_default(),
            desired_count: counts.get(i).cloned().unwrap_or_default(),
        })
        .filter(|g| {
            !(g.filter.is_empty()
                && g.binning.is_empty()
                && g.exposure.is_empty()
                && g.desired_count.is_empty())
        })
        .collect()
}

#[derive(Debug, serde::Deserialize)]
pub(crate) struct ActiveQuery {
    active: bool,
}

/// `POST /targets/{slug}/active` — activate (`true`) or pause (`false`).
/// Answers with the refreshed inbox state.
pub(crate) async fn set_active(
    State(state): State<AppState>,
    Path(slug): Path<String>,
    Query(query): Query<ActiveQuery>,
    headers: HeaderMap,
) -> Response {
    let Some(rp) = state.rp() else {
        return respond(super::equipment::no_rp_card("the targets inbox"), &headers);
    };
    let mut fields = Map::new();
    fields.insert("active".to_string(), Value::Bool(query.active));
    let markup = match rp.targets.update_target(&slug, fields).await {
        Ok(()) => inbox_state(rp).await,
        Err(err) => targets_error_card(&err),
    };
    with_push_url(respond(markup, &headers))
}

/// `POST /targets/{slug}/delete` — discard. Answers with the refreshed
/// inbox state.
pub(crate) async fn delete(
    State(state): State<AppState>,
    Path(slug): Path<String>,
    headers: HeaderMap,
) -> Response {
    let Some(rp) = state.rp() else {
        return respond(super::equipment::no_rp_card("the targets inbox"), &headers);
    };
    let markup = match rp.targets.delete_target(&slug).await {
        Ok(()) => inbox_state(rp).await,
        Err(err) => targets_error_card(&err),
    };
    with_push_url(respond(markup, &headers))
}

#[derive(Debug, Default, serde::Deserialize)]
pub(crate) struct GoalRowQuery {
    remove: Option<String>,
}

/// `GET /targets/goal-row` — the goal-editor fragment: a blank row for
/// the "Add goal" beforeend swap, or an empty body for the per-row
/// Remove (`hx-swap="outerHTML"` of the closest row with nothing).
pub(crate) async fn goal_row_fragment(Query(query): Query<GoalRowQuery>) -> Response {
    if query.remove.is_some() {
        return ().into_response();
    }
    goal_row_markup(&Goal::default(), &ConfigJoin::default()).into_response()
}

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
mod tests {
    use std::sync::Arc;

    use super::*;
    use crate::driver_client::{ConfigClient, ConfigClientError};
    use crate::targets_client::MockTargetsClient;
    use crate::AppState;

    // --- the position-angle tri-state (the plan's P4 note) -----------------

    #[test]
    fn blank_angle_clears_to_inherit() {
        assert_eq!(parse_position_angle("").unwrap(), Value::Null);
        assert_eq!(parse_position_angle("   ").unwrap(), Value::Null);
    }

    #[test]
    fn zero_angle_is_explicit_north_up_not_inherit() {
        assert_eq!(parse_position_angle("0").unwrap(), json!(0.0));
    }

    #[test]
    fn numeric_angles_pass_through_unvalidated() {
        // Domain validation ([0, 360), finite) is rp's — even 360 passes
        // here and comes back as rp's own field error.
        assert_eq!(parse_position_angle("121.25").unwrap(), json!(121.25));
        assert_eq!(parse_position_angle("360").unwrap(), json!(360.0));
    }

    #[test]
    fn non_numeric_angles_are_field_errors() {
        assert!(parse_position_angle("north")
            .unwrap_err()
            .contains("not a number"));
        // `"nan".parse::<f64>()` succeeds but JSON cannot carry it.
        assert!(parse_position_angle("nan")
            .unwrap_err()
            .contains("not a number"));
    }

    #[test]
    fn explicit_angle_renders_typed_not_float_noise() {
        assert_eq!(trim_float(254.0), "254");
        assert_eq!(trim_float(121.25), "121.25");
    }

    // --- the goals editor ---------------------------------------------------

    #[test]
    fn goal_rows_zip_in_order_and_blank_rows_drop() {
        let pairs = vec![
            ("goal_filter".to_string(), "Ha".to_string()),
            ("goal_binning".to_string(), "1x1".to_string()),
            ("goal_exposure".to_string(), "5m".to_string()),
            ("goal_count".to_string(), "12".to_string()),
            // The all-empty second row (an unused blank from "Add goal").
            ("goal_filter".to_string(), String::new()),
            ("goal_binning".to_string(), String::new()),
            ("goal_exposure".to_string(), String::new()),
            ("goal_count".to_string(), String::new()),
        ];
        let goals = parse_goal_rows(&pairs).unwrap();
        assert_eq!(goals.len(), 1);
        assert_eq!(
            goals[0],
            json!({"filter": "Ha", "binning": "1x1", "exposure_duration": "5m", "desired_count": 12})
        );
    }

    #[test]
    fn a_bad_frame_count_names_the_row() {
        let pairs = vec![
            ("goal_filter".to_string(), "Ha".to_string()),
            ("goal_binning".to_string(), "1x1".to_string()),
            ("goal_exposure".to_string(), "5m".to_string()),
            ("goal_count".to_string(), "a dozen".to_string()),
        ];
        let error = parse_goal_rows(&pairs).unwrap_err();
        assert!(error.contains("goal 1"), "{error}");
        assert!(error.contains("a dozen"), "{error}");
    }

    #[test]
    fn goals_flag_only_when_the_roster_knows_better() {
        let roster: BTreeSet<String> = ["Ha".to_string(), "OIII".to_string()].into();
        assert!(goal_flagged("SII", &roster));
        assert!(!goal_flagged("Ha", &roster));
        // rp's permissive rule: no filter wheels, no validation, no flags.
        assert!(!goal_flagged("SII", &BTreeSet::new()));
        assert!(!goal_flagged("", &roster));
    }

    // --- the config join ----------------------------------------------------

    #[test]
    fn join_unions_wheels_and_collects_imaging_train_defaults() {
        let config = json!({ "equipment": {
            "filter_wheels": [
                { "id": "a", "filters": ["Ha", "OIII"] },
                { "id": "b", "filters": ["OIII", "SII"] }
            ],
            "optical_trains": [
                { "id": "main", "default_position_angle_degrees": 254.0 },
                { "id": "guide", "purpose": "guiding",
                  "default_position_angle_degrees": 10.0 },
                { "id": "second", "purpose": "imaging" }
            ]
        }});
        let join = config_join(&config);
        assert_eq!(
            join.filter_roster,
            ["Ha", "OIII", "SII"].map(str::to_string).into()
        );
        // Omitted purpose defaults to imaging; guiding trains never hint;
        // a train without a default contributes nothing.
        assert_eq!(join.train_defaults, vec![("main".to_string(), 254.0)]);
    }

    #[test]
    fn pa_hint_names_train_defaults_or_north_up() {
        let join = ConfigJoin {
            filter_roster: BTreeSet::new(),
            train_defaults: vec![("main".to_string(), 254.0)],
        };
        assert_eq!(pa_hint(&join), "blank = inherit — main: 254.0°");
        assert_eq!(
            pa_hint(&ConfigJoin::default()),
            "blank = inherit — north-up (0°)"
        );
    }

    // --- inbox ordering -----------------------------------------------------

    #[test]
    fn pending_sort_newest_first_active_by_name() {
        let row = |slug: &str, active: bool, updated_at: &str| Row {
            slug: slug.to_string(),
            display_name: slug.to_string(),
            active,
            updated_at: updated_at.to_string(),
            ..Row::default()
        };
        let mut rows = vec![
            row("b-active", true, "2026-07-30T00:00:00Z"),
            row("old-pending", false, "2026-07-29T00:00:00Z"),
            row("a-active", true, "2026-07-31T00:00:00Z"),
            row("new-pending", false, "2026-07-31T00:00:00Z"),
        ];
        sort_rows(&mut rows);
        let order: Vec<&str> = rows.iter().map(|r| r.slug.as_str()).collect();
        assert_eq!(
            order,
            ["new-pending", "old-pending", "a-active", "b-active"]
        );
    }

    // --- handler states the end-to-end suite can't produce -------------------

    /// A `ConfigClient` whose reads always fail — the targets handlers
    /// short-circuit on the targets client before ever reaching it.
    struct UnusedConfig;

    #[async_trait::async_trait]
    impl ConfigClient for UnusedConfig {
        async fn get_config(
            &self,
        ) -> Result<rusty_photon_config::actions::ConfigGetResponse, ConfigClientError> {
            Err(ConfigClientError::Transport("unused".to_string()))
        }

        async fn get_schema(
            &self,
        ) -> Result<rusty_photon_config::actions::ConfigSchemaResponse, ConfigClientError> {
            Err(ConfigClientError::Transport("unused".to_string()))
        }

        async fn apply_config(
            &self,
            _config: &Value,
        ) -> Result<rusty_photon_config::actions::ConfigApplyResponse, ConfigClientError> {
            Err(ConfigClientError::Transport("unused".to_string()))
        }
    }

    fn state_with_targets(targets: MockTargetsClient) -> AppState {
        AppState::with_rp_parts(
            Arc::new(UnusedConfig),
            Arc::new(crate::rp_client::MockRpApi::new()),
            Arc::new(crate::probe::MockProbeHttp::new()),
        )
        .with_targets_client(Arc::new(targets))
    }

    async fn body_of(response: Response) -> String {
        let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        String::from_utf8(bytes.to_vec()).unwrap()
    }

    #[tokio::test]
    async fn an_unavailable_store_names_the_safety_gate_too() {
        // rp down and the /mcp safety gate present identically (a failed
        // connect / a 503-refused request), so the one card names both.
        let mut targets = MockTargetsClient::new();
        targets
            .expect_list_targets()
            .returning(|| Err(TargetsError::Unavailable("connection refused".to_string())));
        let response = page(
            axum::extract::State(state_with_targets(targets)),
            HeaderMap::new(),
        )
        .await;
        let html = body_of(response).await;
        assert!(html.contains("safety-gated"), "{html}");
        assert!(html.contains("connection refused"), "{html}");
        assert!(html.contains("targets-unavailable"), "{html}");
    }

    #[tokio::test]
    async fn a_malformed_response_renders_the_error_banner_not_the_gate_card() {
        let mut targets = MockTargetsClient::new();
        targets
            .expect_list_targets()
            .returning(|| Err(TargetsError::Malformed("two content blocks".to_string())));
        let response = page(
            axum::extract::State(state_with_targets(targets)),
            HeaderMap::new(),
        )
        .await;
        let html = body_of(response).await;
        assert!(html.contains("two content blocks"), "{html}");
        assert!(!html.contains("targets-unavailable"), "{html}");
    }

    /// A `ConfigClient` returning a minimal effective config — the save-path
    /// tests need the review re-render's config join to succeed.
    struct EmptyConfig;

    #[async_trait::async_trait]
    impl ConfigClient for EmptyConfig {
        async fn get_config(
            &self,
        ) -> Result<rusty_photon_config::actions::ConfigGetResponse, ConfigClientError> {
            Ok(rusty_photon_config::actions::ConfigGetResponse {
                config: json!({}),
                overrides: Vec::new(),
            })
        }

        async fn get_schema(
            &self,
        ) -> Result<rusty_photon_config::actions::ConfigSchemaResponse, ConfigClientError> {
            Err(ConfigClientError::Transport("unused".to_string()))
        }

        async fn apply_config(
            &self,
            _config: &Value,
        ) -> Result<rusty_photon_config::actions::ConfigApplyResponse, ConfigClientError> {
            Err(ConfigClientError::Transport("unused".to_string()))
        }
    }

    fn state_with_targets_and_config(targets: MockTargetsClient) -> AppState {
        AppState::with_rp_parts(
            Arc::new(EmptyConfig),
            Arc::new(crate::rp_client::MockRpApi::new()),
            Arc::new(crate::probe::MockProbeHttp::new()),
        )
        .with_targets_client(Arc::new(targets))
    }

    /// The stored row the save-path mocks serve from `get_target`.
    fn stored_target() -> Value {
        json!({
            "slug": "m-82",
            "display_name": "M 82",
            "active": false,
            "coord": { "ra_hours": 5.5, "dec_degrees": 10.0 },
            "priority": 0,
            "goals": [],
            "created_by": "operator",
            "updated_by": "operator",
            "updated_at": "2026-07-31T00:00:00Z"
        })
    }

    fn base_form() -> Vec<(String, String)> {
        vec![
            ("display_name".to_string(), "M 82".to_string()),
            ("priority".to_string(), "0".to_string()),
            ("position_angle_degrees".to_string(), String::new()),
            ("notes".to_string(), String::new()),
        ]
    }

    async fn run_save(targets: MockTargetsClient, form: Vec<(String, String)>) -> String {
        let response = save(
            axum::extract::State(state_with_targets_and_config(targets)),
            Path("m-82".to_string()),
            HeaderMap::new(),
            Form(form),
        )
        .await;
        body_of(response).await
    }

    #[tokio::test]
    async fn an_empty_display_name_is_a_field_error_and_nothing_is_written() {
        // No expect_update_target / expect_set_goals: a write would panic
        // the mock, proving BFF-side validation short-circuits the save.
        let mut targets = MockTargetsClient::new();
        targets
            .expect_get_target()
            .returning(|_| Ok(stored_target()));
        let mut form = base_form();
        form[0].1 = String::new();
        let html = run_save(targets, form).await;
        assert!(html.contains("a display name is required"), "{html}");
    }

    #[tokio::test]
    async fn a_bad_frame_count_renders_the_problem_banner() {
        let mut targets = MockTargetsClient::new();
        targets
            .expect_get_target()
            .returning(|_| Ok(stored_target()));
        let mut form = base_form();
        form.extend([
            ("goal_filter".to_string(), "Ha".to_string()),
            ("goal_binning".to_string(), "1x1".to_string()),
            ("goal_exposure".to_string(), "5m".to_string()),
            ("goal_count".to_string(), "a dozen".to_string()),
        ]);
        let html = run_save(targets, form).await;
        assert!(html.contains("not a whole number"), "{html}");
        // The submitted row survives the re-render for correction.
        assert!(html.contains("a dozen"), "{html}");
    }

    #[tokio::test]
    async fn a_non_pa_tool_rejection_renders_as_the_banner() {
        let mut targets = MockTargetsClient::new();
        targets
            .expect_get_target()
            .returning(|_| Ok(stored_target()));
        targets
            .expect_update_target()
            .returning(|_, _| Err(TargetsError::Tool("display_name: too long".to_string())));
        let html = run_save(targets, base_form()).await;
        assert!(html.contains("display_name: too long"), "{html}");
    }

    #[tokio::test]
    async fn a_goals_rejection_after_a_saved_update_says_so_honestly() {
        let mut targets = MockTargetsClient::new();
        targets
            .expect_get_target()
            .returning(|_| Ok(stored_target()));
        targets.expect_update_target().returning(|_, _| Ok(()));
        targets
            .expect_set_goals()
            .returning(|_, _| Err(TargetsError::Tool("unknown filter `Xx`".to_string())));
        let html = run_save(targets, base_form()).await;
        assert!(html.contains("Details saved; goals were not"), "{html}");
        assert!(html.contains("unknown filter `Xx`"), "{html}");
    }

    #[tokio::test]
    async fn a_successful_save_sends_the_blank_angle_as_an_explicit_null() {
        let mut targets = MockTargetsClient::new();
        targets
            .expect_get_target()
            .returning(|_| Ok(stored_target()));
        targets
            .expect_update_target()
            .withf(|slug, fields| {
                slug == "m-82"
                    && fields.get("position_angle_degrees") == Some(&Value::Null)
                    && fields.get("display_name") == Some(&json!("M 82"))
            })
            .returning(|_, _| Ok(()));
        targets
            .expect_set_goals()
            .withf(|slug, goals| slug == "m-82" && goals.is_empty())
            .returning(|_, _| Ok(()));
        let html = run_save(targets, base_form()).await;
        assert!(html.contains("Saved."), "{html}");
    }

    #[tokio::test]
    async fn a_review_of_an_unavailable_store_renders_the_gate_card() {
        let mut targets = MockTargetsClient::new();
        targets
            .expect_get_target()
            .returning(|_| Err(TargetsError::Unavailable("refused".to_string())));
        let response = review(
            axum::extract::State(state_with_targets_and_config(targets)),
            Path("m-82".to_string()),
            HeaderMap::new(),
        )
        .await;
        let html = body_of(response).await;
        assert!(html.contains("targets-unavailable"), "{html}");
    }

    #[tokio::test]
    async fn activate_and_delete_rejections_render_the_error_card() {
        let mut targets = MockTargetsClient::new();
        targets
            .expect_update_target()
            .returning(|_, _| Err(TargetsError::Tool("unknown target".to_string())));
        let response = set_active(
            axum::extract::State(state_with_targets_and_config(targets)),
            Path("m-82".to_string()),
            Query(ActiveQuery { active: true }),
            HeaderMap::new(),
        )
        .await;
        assert!(body_of(response).await.contains("unknown target"));

        let mut targets = MockTargetsClient::new();
        targets
            .expect_delete_target()
            .returning(|_| Err(TargetsError::Tool("unknown target".to_string())));
        let response = delete(
            axum::extract::State(state_with_targets_and_config(targets)),
            Path("m-82".to_string()),
            HeaderMap::new(),
        )
        .await;
        assert!(body_of(response).await.contains("unknown target"));
    }

    #[tokio::test]
    async fn every_targets_handler_renders_the_no_rp_card_without_an_rp() {
        // `AppState::with_client` carries no rp state — the shape the
        // stubbed-driver unit states use.
        let state = || axum::extract::State(AppState::with_client("stub", Arc::new(UnusedConfig)));
        let no_rp = "No rp orchestrator is configured";
        let html = body_of(page(state(), HeaderMap::new()).await).await;
        assert!(html.contains(no_rp), "{html}");
        let html = body_of(review(state(), Path("x".to_string()), HeaderMap::new()).await).await;
        assert!(html.contains(no_rp), "{html}");
        let html = body_of(
            save(
                state(),
                Path("x".to_string()),
                HeaderMap::new(),
                Form(Vec::new()),
            )
            .await,
        )
        .await;
        assert!(html.contains(no_rp), "{html}");
        let html = body_of(
            set_active(
                state(),
                Path("x".to_string()),
                Query(ActiveQuery { active: true }),
                HeaderMap::new(),
            )
            .await,
        )
        .await;
        assert!(html.contains(no_rp), "{html}");
        let html = body_of(delete(state(), Path("x".to_string()), HeaderMap::new()).await).await;
        assert!(html.contains(no_rp), "{html}");
    }

    #[tokio::test]
    async fn the_goal_row_fragment_serves_a_blank_row_or_nothing() {
        let added = goal_row_fragment(Query(GoalRowQuery::default())).await;
        let html = body_of(added).await;
        assert!(html.contains(r#"name="goal_filter""#), "{html}");
        let removed = goal_row_fragment(Query(GoalRowQuery {
            remove: Some("1".to_string()),
        }))
        .await;
        assert_eq!(body_of(removed).await, "");
    }
}
