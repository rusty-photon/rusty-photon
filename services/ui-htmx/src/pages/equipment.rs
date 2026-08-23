//! The equipment page (`/equipment`) — rp's roster with live connection
//! state, capability tiers, and roster-entry editing.
//!
//! Add, edit, and remove work by config surgery over rp's REST config
//! API (see `docs/services/ui-htmx.md` "Equipment page").
//!
//! The htmx swap unit is `#equipment-page`: the roster view and the add/edit
//! form views are alternate states of that one element. Mutation forms
//! `hx-post` back to their own URL; a successful mutation answers with the
//! refreshed roster state (plus `HX-Push-Url: /equipment`), a rejected one
//! answers with the form state re-rendered with field errors.

use std::sync::Arc;

use axum::extract::{Form, Path, State};
use axum::http::{HeaderMap, HeaderValue};
use axum::response::{IntoResponse, Response};
use maud::{html, Markup};
use serde_json::Value;
use strum::VariantArray;

use crate::driver_client::{ApplyStatus, ConfigClientError, FieldError};
use crate::pages::{self, layout_with_nav, FieldModel, NavTab, Page};
use crate::probe::{self, Tier};
use crate::roster::{self, EquipKind, RosterEntry};
use crate::rp_client::EquipmentStatus;
use crate::{AppState, RpState};

/// Page title for the equipment routes.
const TITLE: &str = "rusty-photon · equipment";

/// One rendered roster row: the config entry joined with live state + tier.
struct RosterRow {
    entry: RosterEntry,
    /// Live `connected` from `GET /api/equipment`; `None` when rp doesn't know
    /// the device (config changed since rp last started).
    connected: Option<bool>,
    tier: Tier,
}

/// The outcome banner over the roster state.
enum EquipBanner {
    /// A mutation persisted; rp reports these paths take effect on restart.
    SavedRestart(Vec<String>),
    /// A mutation persisted with nothing changed (e.g. an identical edit).
    Saved,
    /// A mutation could not be applied (surgery conflict, or rp rejected a
    /// delete-side validation like a dangling reference).
    Problem(String),
}

fn is_htmx(headers: &HeaderMap) -> bool {
    headers.contains_key("HX-Request")
}

/// Wrap an `#equipment-page` fragment in the full page unless htmx asked.
fn respond(fragment: Markup, headers: &HeaderMap) -> Response {
    if is_htmx(headers) {
        fragment.into_response()
    } else {
        layout_with_nav(TITLE, NavTab::Equipment, &fragment).into_response()
    }
}

/// The shared "no rp configured" state (also used by the stream page).
pub(crate) fn no_rp_card(what: &str) -> Markup {
    html! {
        div #equipment-page.card {
            div class="banner error" {
                span.dot {}
                span {
                    (format!("No rp orchestrator is configured, so {what} is unavailable. "))
                    "Add an `rp` target to the ui-htmx config file."
                }
            }
        }
    }
}

/// An error card for a failed rp call, with a retry back to the roster.
fn rp_error_card(err: &ConfigClientError) -> Markup {
    html! {
        div #equipment-page.card {
            div class="banner error" { span.dot {} span { (err.to_string()) } }
            p {
                button.link type="button" hx-get="/equipment"
                    hx-target="#equipment-page" hx-swap="outerHTML" { "Retry" }
            }
        }
    }
}

// --- the roster state -----------------------------------------------------------

/// Probe every entry concurrently; the page render is bounded by roughly one
/// probe timeout, not the roster size. Entries whose probe task fails keep
/// [`Tier::Unreachable`].
async fn probe_all(rp: &RpState, entries: &[RosterEntry]) -> Vec<Tier> {
    let mut tiers = vec![Tier::Unreachable; entries.len()];
    let mut set = tokio::task::JoinSet::new();
    for (index, entry) in entries.iter().enumerate() {
        let http = Arc::clone(&rp.probe_http);
        let url = entry.alpaca_url.clone();
        let device_type = entry.kind.ascom_type();
        let device_number = entry.device_number;
        set.spawn(async move {
            (
                index,
                probe::probe_device(&http, &url, device_type, device_number).await,
            )
        });
    }
    while let Some(joined) = set.join_next().await {
        if let Ok((index, tier)) = joined {
            if let Some(slot) = tiers.get_mut(index) {
                *slot = tier;
            }
        }
    }
    tiers
}

/// Fetch rp's config + live status, join, probe, and render the roster state.
async fn roster_state(rp: &RpState, banner: Option<EquipBanner>) -> Markup {
    let config = match rp.config_client.get_config().await {
        Ok(resp) => resp.config,
        Err(err) => return rp_error_card(&err),
    };
    // Live status is best-effort: rp answered the config call, so a status
    // failure (mid-restart blip) degrades LEDs to "unknown" rather than
    // failing the page.
    let status = rp.api.equipment_status().await.unwrap_or_default();
    let entries = roster::parse_roster(&config);
    let tiers = probe_all(rp, &entries).await;
    let rows: Vec<RosterRow> = entries
        .into_iter()
        .zip(tiers)
        .map(|(entry, tier)| RosterRow {
            connected: connected_of(&status, &entry),
            entry,
            tier,
        })
        .collect();
    roster_markup(&rows, banner)
}

fn connected_of(status: &EquipmentStatus, entry: &RosterEntry) -> Option<bool> {
    status.connected(entry.kind.config_key(), &entry.id)
}

const fn led_class(connected: Option<bool>) -> &'static str {
    match connected {
        Some(true) => "led ok",
        Some(false) => "led bad",
        None => "led unknown",
    }
}

fn tier_badge(tier: &Tier) -> Markup {
    let (class, label, hint) = match tier {
        Tier::Managed => ("managed", "managed", None),
        Tier::SetupPage(_) => ("setup", "setup page", None),
        Tier::ControlOnly => ("control", "control only", None),
        Tier::AuthRequired => (
            "auth",
            "auth required",
            Some("This device requires credentials the BFF does not hold — configure it from its own setup UI or its config file."),
        ),
        Tier::Unreachable => ("unreachable", "unreachable", None),
    };
    html! {
        span class=(format!("tier-badge {class}")) title=[hint] { (label) }
    }
}

fn row_markup(row: &RosterRow) -> Markup {
    let entry = &row.entry;
    let kind_key = entry.kind.config_key();
    let edit_href = format!("/equipment/{kind_key}/{}/edit", entry.id);
    let delete_action = format!("/equipment/{kind_key}/{}/delete", entry.id);
    html! {
        li.equip-row id=(format!("row-{kind_key}-{}", entry.id)) {
            span class=(led_class(row.connected)) {}
            span.dev-name { (entry.display_name()) }
            span.svc-id { (entry.id) }
            span.addr.mono { (entry.alpaca_url) }
            (tier_badge(&row.tier))
            span.row-actions {
                @if matches!(row.tier, Tier::Managed) {
                    a.configure href=(format!("/config/{}", entry.service_key())) { "Configure" }
                }
                @if let Tier::SetupPage(url) = &row.tier {
                    a.setup-link href=(url) target="_blank" rel="noopener" { "Setup page" }
                }
                a.edit href=(edit_href) { "Edit" }
                // JS-required htmx button (plan §7); hx-confirm guards the
                // destructive action with the browser's confirm dialog.
                button.link.danger type="button" hx-post=(delete_action)
                    hx-confirm=(format!("Remove {} \"{}\" from rp's roster?", entry.kind.ascom_type(), entry.id))
                    hx-target="#equipment-page" hx-swap="outerHTML" { "Remove" }
            }
        }
    }
}

fn banner_markup(banner: &EquipBanner) -> Markup {
    match banner {
        EquipBanner::SavedRestart(paths) => html! {
            div class="banner warn" {
                span.dot {}
                span {
                    "Saved to rp's config. These changes take effect when rp is restarted: "
                    span.mono { (paths.join(", ")) }
                }
            }
        },
        EquipBanner::Saved => html! {
            div class="banner ok" { span.dot {} span { "Saved. Nothing changed." } }
        },
        EquipBanner::Problem(msg) => html! {
            div class="banner error" { span.dot {} span { (msg) } }
        },
    }
}

fn roster_markup(rows: &[RosterRow], banner: Option<EquipBanner>) -> Markup {
    let has_mount = rows.iter().any(|r| r.entry.kind == EquipKind::Mount);
    html! {
        div #equipment-page {
            @if let Some(b) = banner { (banner_markup(&b)) }
            h1 { "Equipment" }
            p.subtitle {
                "rp's equipment roster — live state, per-device capability, and "
                "the roster entries themselves (stored in rp's config)."
            }
            @for kind in EquipKind::VARIANTS.iter().copied() {
                @let kind_rows: Vec<&RosterRow> =
                    rows.iter().filter(|r| r.entry.kind == kind).collect();
                section.equip-kind id=(format!("kind-{}", kind.config_key())) {
                    h2.section-head {
                        (kind.display())
                        @if !kind.is_singular() || !has_mount {
                            a.add-entry href=(format!("/equipment/{}/new", kind.config_key())) {
                                (format!("+ add {}", kind.ascom_type()))
                            }
                        }
                    }
                    @if kind_rows.is_empty() {
                        p.subtitle.empty-kind { "None configured." }
                    } @else {
                        ul.equip-list {
                            @for row in kind_rows { (row_markup(row)) }
                        }
                    }
                }
            }
        }
    }
}

// --- the form state -------------------------------------------------------------

enum FormMode<'a> {
    Add,
    Edit { id: &'a str },
}

/// Render the add/edit form for one entry of `kind`. Reuses the config pages'
/// field renderer (names are entry-relative dotted paths) plus the hidden
/// round-trip blobs `merge_form` expects. Composite subtrees (an entry's
/// optional `auth`) are skipped by the walker and round-trip via the blob —
/// same rule as the config pages.
fn entry_form(
    kind: EquipKind,
    mode: &FormMode<'_>,
    model: &FieldModel,
    values: &Value,
    errors: &[FieldError],
    problem: Option<&str>,
) -> Markup {
    let kind_key = kind.config_key();
    let (action, heading) = match mode {
        FormMode::Add => (
            format!("/equipment/{kind_key}/new"),
            format!("Add {}", kind.ascom_type()),
        ),
        FormMode::Edit { id } => (
            format!("/equipment/{kind_key}/{id}/edit"),
            format!("Edit {} \"{id}\"", kind.ascom_type()),
        ),
    };
    let page = Page {
        service: "equipment",
        title: &heading,
        subtitle: kind_key,
        // Equipment forms edit roster entries in rp's config — there is no
        // process here for Sentinel to restart.
        can_restart: false,
    };
    let ctx = super::FieldCtx {
        model,
        config: values,
        overrides: &[],
        unlocked: &[],
        errors,
    };
    let config_blob = super::canonical_json(values);
    html! {
        div #equipment-page.card {
            @if let Some(msg) = problem {
                div class="banner error" { span.dot {} span { (msg) } }
            }
            h1 { (heading) }
            p.subtitle {
                "Stored in rp's config. Composite fields (e.g. device credentials) "
                "are edited in rp's config file."
            }
            form hx-post=(action) hx-target="#equipment-page" hx-swap="outerHTML" {
                input type="hidden" name="__config" value=(config_blob);
                input type="hidden" name="__overrides" value="[]";
                input type="hidden" name="__unlocked" value="[]";
                fieldset {
                    @for spec in model.field_specs() { (super::render_field(&page, &ctx, spec)) }
                }
                div.actions {
                    button.primary type="submit" { "Save" }
                    a href="/equipment" { "Cancel" }
                }
            }
        }
    }
}

// --- shared handler plumbing ------------------------------------------------

/// Everything a mutation handler needs from the path + state, or the early
/// response (no rp / unknown kind).
fn kind_or_response(
    state: &AppState,
    kind_key: &str,
    headers: &HeaderMap,
) -> Result<(Arc<RpState>, EquipKind), Box<Response>> {
    let Some(rp) = state.rp() else {
        return Err(Box::new(respond(no_rp_card("the equipment page"), headers)));
    };
    let Some(kind) = EquipKind::from_key(kind_key) else {
        return Err(Box::new(respond(
            html! {
                div #equipment-page.card {
                    div class="banner error" {
                        span.dot {}
                        span { (format!("No equipment kind named \"{kind_key}\".")) }
                    }
                    p { a href="/equipment" { "Back to equipment" } }
                }
            },
            headers,
        )));
    };
    Ok((Arc::clone(rp), kind))
}

/// Fetch rp's schema and build the per-entry model for `kind`.
async fn item_model(rp: &RpState, kind: EquipKind) -> Result<FieldModel, Markup> {
    let schema = match rp.config_client.get_schema().await {
        Ok(schema) => schema,
        Err(err) => return Err(rp_error_card(&err)),
    };
    FieldModel::from_item_schema(&schema, kind.config_key()).ok_or_else(|| {
        rp_error_card(&ConfigClientError::Decode(format!(
            "rp's config schema carries no {} entry shape",
            kind.config_key()
        )))
    })
}

/// A null-leaf skeleton for the add form: every model leaf exists (as `null`)
/// so `merge_form`'s pointer writes land; nulls left after the merge (an
/// untouched optional) are stripped so serde defaults apply on the rp side.
fn skeleton_of(model: &FieldModel) -> Value {
    let mut root = serde_json::Map::new();
    for spec in model.field_specs() {
        insert_null_leaf(&mut root, &spec.pointer_segments());
    }
    Value::Object(root)
}

fn insert_null_leaf(map: &mut serde_json::Map<String, Value>, segments: &[&str]) {
    let [first, rest @ ..] = segments else { return };
    if rest.is_empty() {
        map.entry((*first).to_string()).or_insert(Value::Null);
        return;
    }
    let child = map
        .entry((*first).to_string())
        .or_insert_with(|| Value::Object(serde_json::Map::new()));
    if let Value::Object(child_map) = child {
        insert_null_leaf(child_map, rest);
    }
}

/// Drop unset object members recursively (post-merge cleanup for entry forms):
/// `null` (an untouched skeleton leaf) and the **empty string** (a text input
/// the operator left blank — "use rp's default", e.g. an optional humantime
/// `poll_interval`; sending `""` would fail rp's typed parse). A required
/// field dropped this way surfaces as rp's "missing field" message (or the
/// BFF's own `id` check).
fn strip_unset(value: &mut Value) {
    if let Value::Object(map) = value {
        map.retain(|_, v| !v.is_null() && v.as_str() != Some(""));
        for v in map.values_mut() {
            strip_unset(v);
        }
    }
}

/// Apply a mutated config to rp and render the outcome:
/// - `invalid` → the form state re-rendered with the entry's field errors
///   (paths re-anchored via `error_prefix`), values preserved;
/// - `ok` → the refreshed roster state with the restart callout;
/// - transport error → the rp error card.
///
/// The `bool` is true when the response landed on the **roster** state (the
/// caller then pushes `/equipment` as the browser URL; a re-rendered form must
/// keep its own URL).
#[allow(clippy::too_many_arguments)]
async fn apply_and_render(
    rp: &RpState,
    mutated: &Value,
    kind: EquipKind,
    mode: FormMode<'_>,
    model: &FieldModel,
    submitted_entry: &Value,
    error_prefix: &str,
) -> (Markup, bool) {
    match rp.config_client.apply_config(mutated).await {
        Err(err) => (rp_error_card(&err), true),
        Ok(resp) => match resp.status {
            ApplyStatus::Invalid => {
                let errors = roster::relativize_errors(resp.errors, error_prefix);
                (
                    entry_form(kind, &mode, model, submitted_entry, &errors, None),
                    false,
                )
            }
            // rp never reloads (ApplyDisposition::Restart) — an `applying`
            // status can't occur; treat it like `ok` defensively.
            ApplyStatus::Ok | ApplyStatus::Applying => {
                let banner = if resp.restart_required.is_empty() {
                    EquipBanner::Saved
                } else {
                    EquipBanner::SavedRestart(resp.restart_required)
                };
                (roster_state(rp, Some(banner)).await, true)
            }
        },
    }
}

// --- handlers ---------------------------------------------------------------

/// `GET /equipment`.
pub async fn page(State(state): State<AppState>, headers: HeaderMap) -> Response {
    let Some(rp) = state.rp() else {
        return respond(no_rp_card("the equipment page"), &headers);
    };
    respond(roster_state(rp, None).await, &headers)
}

/// `GET /equipment/{kind}/new`.
pub async fn new_form(
    State(state): State<AppState>,
    Path(kind_key): Path<String>,
    headers: HeaderMap,
) -> Response {
    let (rp, kind) = match kind_or_response(&state, &kind_key, &headers) {
        Ok(parts) => parts,
        Err(response) => return *response,
    };
    let model = match item_model(&rp, kind).await {
        Ok(model) => model,
        Err(card) => return respond(card, &headers),
    };
    let values = skeleton_of(&model);
    respond(
        entry_form(kind, &FormMode::Add, &model, &values, &[], None),
        &headers,
    )
}

/// Build the submitted entry from the form via the shared merge machinery.
/// The hidden `__config` blob is re-seeded onto the model's null-leaf skeleton
/// first: `merge_form` writes by JSON pointer into *existing* slots, so every
/// model leaf must exist for the overlay to land — the rendered form carries
/// the skeleton, but a sparse (hand-crafted) blob must not silently drop the
/// submitted fields.
fn entry_from_form(
    mut form: pages::FormValues,
    model: &FieldModel,
) -> Result<(Value, Vec<FieldError>), String> {
    let blob: Value = match form.get("__config") {
        Some(raw) => serde_json::from_str(raw).map_err(|e| e.to_string())?,
        None => Value::Object(serde_json::Map::new()),
    };
    let mut seeded = skeleton_of(model);
    merge_values(&mut seeded, &blob);
    form.set(
        "__config",
        serde_json::to_string(&seeded).map_err(|e| e.to_string())?,
    );
    match pages::merge_form(&form, model) {
        Ok(merged) => {
            let mut entry = merged.config;
            strip_unset(&mut entry);
            Ok((entry, merged.errors))
        }
        Err(err) => Err(err.to_string()),
    }
}

/// Deep-merge `overlay` into `base`: overlay object members recurse, anything
/// else replaces. (The blob wins over the skeleton's nulls; skeleton leaves
/// absent from the blob stay null for `merge_form` to fill.)
fn merge_values(base: &mut Value, overlay: &Value) {
    match (base, overlay) {
        (Value::Object(base_map), Value::Object(overlay_map)) => {
            for (key, value) in overlay_map {
                match base_map.get_mut(key) {
                    Some(slot) => merge_values(slot, value),
                    None => {
                        base_map.insert(key.clone(), value.clone());
                    }
                }
            }
        }
        (base_slot, other) => *base_slot = other.clone(),
    }
}

/// The one BFF-side required field: a list entry's `id` is the roster key.
fn require_id(kind: EquipKind, entry: &Value, errors: &mut Vec<FieldError>) {
    if kind.is_singular() {
        return;
    }
    let present = entry
        .get("id")
        .and_then(Value::as_str)
        .is_some_and(|id| !id.trim().is_empty());
    if !present {
        errors.push(FieldError {
            path: "id".to_string(),
            msg: "required — the roster is keyed by this id".to_string(),
        });
    }
}

/// `POST /equipment/{kind}/new`.
pub async fn new_submit(
    State(state): State<AppState>,
    Path(kind_key): Path<String>,
    headers: HeaderMap,
    // Pairs, not a map: a checkbox group (e.g. a camera's
    // cooler_targets_c grid) posts one pair per checked box and
    // `serde_urlencoded` would collapse duplicate keys in a map.
    Form(form): Form<Vec<(String, String)>>,
) -> Response {
    let form = pages::FormValues::from(form);
    let (rp, kind) = match kind_or_response(&state, &kind_key, &headers) {
        Ok(parts) => parts,
        Err(response) => return *response,
    };
    let model = match item_model(&rp, kind).await {
        Ok(model) => model,
        Err(card) => return respond(card, &headers),
    };
    let (entry, mut errors) = match entry_from_form(form, &model) {
        Ok(parts) => parts,
        Err(msg) => return respond(rp_error_card(&ConfigClientError::Decode(msg)), &headers),
    };
    require_id(kind, &entry, &mut errors);
    if !errors.is_empty() {
        return respond(
            entry_form(kind, &FormMode::Add, &model, &entry, &errors, None),
            &headers,
        );
    }
    let mut config = match rp.config_client.get_config().await {
        Ok(resp) => resp.config,
        Err(err) => return respond(rp_error_card(&err), &headers),
    };
    let prefix = match roster::insert_entry(&mut config, kind, entry.clone()) {
        Ok(prefix) => prefix,
        Err(err) => {
            return respond(
                entry_form(
                    kind,
                    &FormMode::Add,
                    &model,
                    &entry,
                    &[],
                    Some(&err.to_string()),
                ),
                &headers,
            )
        }
    };
    let (card, to_roster) =
        apply_and_render(&rp, &config, kind, FormMode::Add, &model, &entry, &prefix).await;
    let response = respond(card, &headers);
    if to_roster {
        with_push_url(response)
    } else {
        response
    }
}

/// `GET /equipment/{kind}/{id}/edit`.
pub async fn edit_form(
    State(state): State<AppState>,
    Path((kind_key, id)): Path<(String, String)>,
    headers: HeaderMap,
) -> Response {
    let (rp, kind) = match kind_or_response(&state, &kind_key, &headers) {
        Ok(parts) => parts,
        Err(response) => return *response,
    };
    let model = match item_model(&rp, kind).await {
        Ok(model) => model,
        Err(card) => return respond(card, &headers),
    };
    let config = match rp.config_client.get_config().await {
        Ok(resp) => resp.config,
        Err(err) => return respond(rp_error_card(&err), &headers),
    };
    let Some(entry) = roster::find_entry(&config, kind, &id) else {
        return respond(
            rp_error_card(&ConfigClientError::Decode(format!(
                "no {} entry with id \"{id}\" in rp's roster",
                kind.config_key()
            ))),
            &headers,
        );
    };
    respond(
        entry_form(
            kind,
            &FormMode::Edit { id: &id },
            &model,
            &entry.raw,
            &[],
            None,
        ),
        &headers,
    )
}

/// `POST /equipment/{kind}/{id}/edit`.
pub async fn edit_submit(
    State(state): State<AppState>,
    Path((kind_key, id)): Path<(String, String)>,
    headers: HeaderMap,
    Form(form): Form<Vec<(String, String)>>,
) -> Response {
    let form = pages::FormValues::from(form);
    let (rp, kind) = match kind_or_response(&state, &kind_key, &headers) {
        Ok(parts) => parts,
        Err(response) => return *response,
    };
    let model = match item_model(&rp, kind).await {
        Ok(model) => model,
        Err(card) => return respond(card, &headers),
    };
    let mode = FormMode::Edit { id: &id };
    let (entry, mut errors) = match entry_from_form(form, &model) {
        Ok(parts) => parts,
        Err(msg) => return respond(rp_error_card(&ConfigClientError::Decode(msg)), &headers),
    };
    require_id(kind, &entry, &mut errors);
    if !errors.is_empty() {
        return respond(
            entry_form(kind, &mode, &model, &entry, &errors, None),
            &headers,
        );
    }
    let mut config = match rp.config_client.get_config().await {
        Ok(resp) => resp.config,
        Err(err) => return respond(rp_error_card(&err), &headers),
    };
    let prefix = match roster::replace_entry(&mut config, kind, &id, entry.clone()) {
        Ok(prefix) => prefix,
        Err(err) => {
            return respond(
                entry_form(kind, &mode, &model, &entry, &[], Some(&err.to_string())),
                &headers,
            )
        }
    };
    let (card, to_roster) =
        apply_and_render(&rp, &config, kind, mode, &model, &entry, &prefix).await;
    let response = respond(card, &headers);
    if to_roster {
        with_push_url(response)
    } else {
        response
    }
}

/// `POST /equipment/{kind}/{id}/delete`.
pub async fn delete(
    State(state): State<AppState>,
    Path((kind_key, id)): Path<(String, String)>,
    headers: HeaderMap,
) -> Response {
    let (rp, kind) = match kind_or_response(&state, &kind_key, &headers) {
        Ok(parts) => parts,
        Err(response) => return *response,
    };
    let mut config = match rp.config_client.get_config().await {
        Ok(resp) => resp.config,
        Err(err) => return respond(rp_error_card(&err), &headers),
    };
    if let Err(err) = roster::remove_entry(&mut config, kind, &id) {
        let card = roster_state(&rp, Some(EquipBanner::Problem(err.to_string()))).await;
        return respond(card, &headers);
    }
    let card = match rp.config_client.apply_config(&config).await {
        Err(err) => rp_error_card(&err),
        Ok(resp) => match resp.status {
            // rp rejected the removal (e.g. another entry references this id) —
            // surface the field errors as a roster banner; nothing persisted.
            ApplyStatus::Invalid => {
                let msgs: Vec<String> = resp
                    .errors
                    .iter()
                    .map(|e| format!("{}: {}", e.path, e.msg))
                    .collect();
                roster_state(&rp, Some(EquipBanner::Problem(msgs.join("; ")))).await
            }
            ApplyStatus::Ok | ApplyStatus::Applying => {
                let banner = if resp.restart_required.is_empty() {
                    EquipBanner::Saved
                } else {
                    EquipBanner::SavedRestart(resp.restart_required)
                };
                roster_state(&rp, Some(banner)).await
            }
        },
    };
    with_push_url(respond(card, &headers))
}

/// After a successful htmx mutation the browser should show `/equipment`
/// regardless of which mutation URL was posted.
fn with_push_url(mut response: Response) -> Response {
    response
        .headers_mut()
        .insert("HX-Push-Url", HeaderValue::from_static("/equipment"));
    response
}

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
mod tests {
    use std::sync::Mutex;

    use serde_json::json;

    use super::*;
    use crate::driver_client::{
        ApplyStatus, ConfigApplyResponse, ConfigGetResponse, ConfigSchemaResponse,
    };
    use crate::probe::MockProbeHttp;
    use crate::rp_client::MockRpApi;

    /// A canned rp: serves a fixed config + schema, records applies, and
    /// answers them with the restart classification (`ApplyDisposition::Restart`)
    /// or a fixed invalid response.
    struct RpStub {
        config: Mutex<Value>,
        invalid: Option<Vec<FieldError>>,
        applied: Mutex<Vec<Value>>,
    }

    impl RpStub {
        fn new(config: Value) -> Self {
            Self {
                config: Mutex::new(config),
                invalid: None,
                applied: Mutex::new(Vec::new()),
            }
        }

        fn rejecting(config: Value, errors: Vec<FieldError>) -> Self {
            Self {
                invalid: Some(errors),
                ..Self::new(config)
            }
        }
    }

    #[async_trait::async_trait]
    impl crate::driver_client::ConfigClient for RpStub {
        async fn get_config(&self) -> Result<ConfigGetResponse, ConfigClientError> {
            Ok(ConfigGetResponse {
                config: self.config.lock().unwrap().clone(),
                overrides: vec![],
            })
        }
        async fn get_schema(&self) -> Result<ConfigSchemaResponse, ConfigClientError> {
            Ok(ConfigSchemaResponse {
                schema: json!({
                    "$defs": {
                        "CoverCalibratorConfig": {
                            "type": "object",
                            "properties": {
                                "id": { "type": "string" },
                                "alpaca_url": { "type": "string" },
                                "device_number": { "type": "integer", "format": "uint32", "minimum": 0 },
                            },
                        },
                    },
                    "type": "object",
                    "properties": {
                        "equipment": { "type": "object", "properties": {
                            "cover_calibrators": {
                                "type": "array",
                                "items": { "$ref": "#/$defs/CoverCalibratorConfig" }
                            },
                        }},
                    },
                }),
                locked_fields: vec![],
                read_only_fields: vec![],
            })
        }
        async fn apply_config(
            &self,
            config: &Value,
        ) -> Result<ConfigApplyResponse, ConfigClientError> {
            self.applied.lock().unwrap().push(config.clone());
            if let Some(errors) = &self.invalid {
                return Ok(ConfigApplyResponse::invalid(errors.clone()));
            }
            *self.config.lock().unwrap() = config.clone();
            Ok(ConfigApplyResponse {
                status: ApplyStatus::Ok,
                applied: vec![],
                reload: vec![],
                restart_required: vec!["equipment".to_string()],
                skipped_override: vec![],
                persisted_to: Some("/tmp/rp.json".to_string()),
                errors: vec![],
            })
        }
    }

    fn rp_config_with_flat_panel() -> Value {
        json!({
            "equipment": {
                "cover_calibrators": [
                    { "id": "flat-panel", "alpaca_url": "http://127.0.0.1:19", "device_number": 0 }
                ],
                "mount": null
            }
        })
    }

    fn state_with(stub: Arc<RpStub>, api: MockRpApi, probe_http: MockProbeHttp) -> AppState {
        AppState::with_rp_parts(stub, Arc::new(api), Arc::new(probe_http))
    }

    fn api_with_status(status: Value) -> MockRpApi {
        let mut api = MockRpApi::new();
        api.expect_equipment_status().returning(move || {
            let status = status.clone();
            Box::pin(async move { Ok(serde_json::from_value(status).unwrap()) })
        });
        api
    }

    fn empty_status() -> Value {
        json!({
            "cameras": [], "filter_wheels": [], "cover_calibrators": [],
            "focusers": [], "safety_monitors": [], "switches": [],
            "rotators": [], "observing_conditions": [], "domes": [], "mount": null
        })
    }

    fn probe_managed() -> MockProbeHttp {
        let mut probe_http = MockProbeHttp::new();
        probe_http
            .expect_get()
            .withf(|url| url.contains("/supportedactions"))
            .returning(|_| {
                Box::pin(async {
                    Ok((
                        200,
                        json!({ "Value": ["config.get", "config.apply"] }).to_string(),
                    ))
                })
            });
        probe_http
    }

    async fn body_of(response: Response) -> String {
        let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        String::from_utf8(bytes.to_vec()).unwrap()
    }

    /// Every kind gets a section, in `EquipKind` declaration order, whether or
    /// not it has entries.
    #[test]
    fn roster_renders_one_section_per_kind_in_order() {
        let html = roster_markup(&[], None).into_string();
        let ids: Vec<&str> = html
            .split(r#"id="kind-"#)
            .skip(1)
            .map(|rest| rest.split('"').next().unwrap())
            .collect();
        assert_eq!(
            ids,
            vec![
                "cameras",
                "filter_wheels",
                "cover_calibrators",
                "focusers",
                "safety_monitors",
                "switches",
                "rotators",
                "observing_conditions",
                "domes",
                "mount",
            ],
            "{html}"
        );
    }

    #[tokio::test]
    async fn page_joins_roster_status_and_tier() {
        let status = json!({
            "cameras": [], "filter_wheels": [],
            "cover_calibrators": [ { "id": "flat-panel", "connected": true } ],
            "focusers": [], "safety_monitors": [], "mount": null
        });
        let state = state_with(
            Arc::new(RpStub::new(rp_config_with_flat_panel())),
            api_with_status(status),
            probe_managed(),
        );
        let html = body_of(page(State(state), HeaderMap::new()).await).await;
        assert!(html.contains("row-cover_calibrators-flat-panel"), "{html}");
        assert!(html.contains("led ok"), "{html}");
        assert!(html.contains("tier-badge managed"), "{html}");
        assert!(
            html.contains("/config/rp:cover_calibrators:flat-panel"),
            "{html}"
        );
        // The mount is absent, so its section offers an add affordance.
        assert!(html.contains("/equipment/mount/new"), "{html}");
    }

    #[tokio::test]
    async fn page_without_rp_renders_the_no_rp_card() {
        let state = AppState::with_client("dsd-fp2", Arc::new(RpStub::new(json!({}))));
        let html = body_of(page(State(state), HeaderMap::new()).await).await;
        assert!(html.contains("No rp orchestrator is configured"), "{html}");
    }

    #[tokio::test]
    async fn unknown_device_in_status_renders_unknown_led() {
        // rp's live status doesn't know the device (added after rp started).
        let state = state_with(
            Arc::new(RpStub::new(rp_config_with_flat_panel())),
            api_with_status(empty_status()),
            probe_managed(),
        );
        let html = body_of(page(State(state), HeaderMap::new()).await).await;
        assert!(html.contains("led unknown"), "{html}");
    }

    #[tokio::test]
    async fn new_submit_splices_the_entry_and_renders_the_restart_callout() {
        let stub = Arc::new(RpStub::new(rp_config_with_flat_panel()));
        let state = state_with(
            Arc::clone(&stub),
            api_with_status(empty_status()),
            probe_managed(),
        );
        let form: Vec<(String, String)> = vec![
            ("__config".to_string(), "{}".to_string()),
            ("__overrides".to_string(), "[]".to_string()),
            ("id".to_string(), "new-flat".to_string()),
            ("alpaca_url".to_string(), "http://127.0.0.1:1".to_string()),
            ("device_number".to_string(), "0".to_string()),
        ];

        let response = new_submit(
            State(state),
            Path("cover_calibrators".to_string()),
            HeaderMap::new(),
            Form(form),
        )
        .await;
        assert_eq!(
            response
                .headers()
                .get("HX-Push-Url")
                .map(|v| v.to_str().unwrap()),
            Some("/equipment")
        );
        let html = body_of(response).await;
        assert!(html.contains("when rp is restarted"), "{html}");
        assert!(html.contains("row-cover_calibrators-new-flat"), "{html}");

        let applied = stub.applied.lock().unwrap();
        assert_eq!(applied.len(), 1);
        assert_eq!(
            applied[0]
                .pointer("/equipment/cover_calibrators/1/id")
                .and_then(Value::as_str),
            Some("new-flat")
        );
        // The numeric leaf was coerced, not sent as a string.
        assert_eq!(
            applied[0]
                .pointer("/equipment/cover_calibrators/1/device_number")
                .and_then(Value::as_u64),
            Some(0)
        );
    }

    #[tokio::test]
    async fn new_submit_without_id_re_renders_the_form_with_a_field_error() {
        let stub = Arc::new(RpStub::new(rp_config_with_flat_panel()));
        let state = state_with(
            Arc::clone(&stub),
            api_with_status(empty_status()),
            MockProbeHttp::new(),
        );
        let form: Vec<(String, String)> = vec![
            ("__config".to_string(), "{}".to_string()),
            ("__overrides".to_string(), "[]".to_string()),
            ("alpaca_url".to_string(), "http://127.0.0.1:1".to_string()),
        ];

        let response = new_submit(
            State(state),
            Path("cover_calibrators".to_string()),
            HeaderMap::new(),
            Form(form),
        )
        .await;
        let html = body_of(response).await;
        assert!(html.contains("the roster is keyed by this id"), "{html}");
        assert!(
            stub.applied.lock().unwrap().is_empty(),
            "nothing must be applied"
        );
    }

    #[tokio::test]
    async fn rejected_apply_relativizes_the_field_error_onto_the_form() {
        let stub = Arc::new(RpStub::rejecting(
            rp_config_with_flat_panel(),
            vec![FieldError {
                path: "equipment.cover_calibrators.1.alpaca_url".to_string(),
                msg: "not a URL".to_string(),
            }],
        ));
        let state = state_with(
            Arc::clone(&stub),
            api_with_status(empty_status()),
            MockProbeHttp::new(),
        );
        let form: Vec<(String, String)> = vec![
            ("__config".to_string(), "{}".to_string()),
            ("__overrides".to_string(), "[]".to_string()),
            ("id".to_string(), "new-flat".to_string()),
            ("alpaca_url".to_string(), "nope".to_string()),
        ];

        let response = new_submit(
            State(state),
            Path("cover_calibrators".to_string()),
            HeaderMap::new(),
            Form(form),
        )
        .await;
        let html = body_of(response).await;
        // The absolute rp path is re-anchored onto the relative form field.
        assert!(html.contains("not a URL"), "{html}");
        assert!(html.contains(r#"name="alpaca_url""#), "{html}");
    }

    #[tokio::test]
    async fn delete_removes_the_entry_and_applies() {
        let stub = Arc::new(RpStub::new(rp_config_with_flat_panel()));
        let state = state_with(
            Arc::clone(&stub),
            api_with_status(empty_status()),
            MockProbeHttp::new(),
        );
        let response = delete(
            State(state),
            Path(("cover_calibrators".to_string(), "flat-panel".to_string())),
            HeaderMap::new(),
        )
        .await;
        let html = body_of(response).await;
        assert!(html.contains("when rp is restarted"), "{html}");
        let applied = stub.applied.lock().unwrap();
        assert_eq!(applied.len(), 1);
        assert_eq!(
            applied[0]
                .pointer("/equipment/cover_calibrators")
                .and_then(Value::as_array)
                .map(Vec::len),
            Some(0)
        );
    }

    #[tokio::test]
    async fn unknown_kind_renders_an_error() {
        let state = state_with(
            Arc::new(RpStub::new(json!({}))),
            MockRpApi::new(),
            MockProbeHttp::new(),
        );
        let response = new_form(
            State(state),
            Path("spectrographs".to_string()),
            HeaderMap::new(),
        )
        .await;
        let html = body_of(response).await;
        assert!(html.contains("No equipment kind named"), "{html}");
    }

    #[test]
    fn skeleton_and_strip_unset_round_trip() {
        let schema = ConfigSchemaResponse {
            schema: json!({
                "type": "object",
                "properties": { "equipment": { "type": "object", "properties": {
                    "cameras": { "type": "array", "items": { "type": "object", "properties": {
                        "id": { "type": "string" },
                        "gain": { "type": "integer" },
                        "poll_interval": { "type": "string" },
                    }}}
                }}},
            }),
            locked_fields: vec![],
            read_only_fields: vec![],
        };
        let model = FieldModel::from_item_schema(&schema, "cameras").unwrap();
        let mut skeleton = skeleton_of(&model);
        assert!(skeleton.pointer("/id").unwrap().is_null());
        assert!(skeleton.pointer("/gain").unwrap().is_null());
        // Untouched null leaves and blank text inputs are both stripped so
        // serde defaults apply rp-side (a "" would fail a typed parse like a
        // humantime poll_interval).
        skeleton["id"] = json!("cam");
        skeleton["poll_interval"] = json!("");
        strip_unset(&mut skeleton);
        assert_eq!(skeleton, json!({ "id": "cam" }));
    }
}
