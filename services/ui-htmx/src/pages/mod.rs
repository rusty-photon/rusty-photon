//! Server-rendered pages and fragments (Maud + HTMX) for the configuration UI,
//! plus the form ⇆ Config mapping the BFF layers over the shared walker.
//!
//! The form is rendered generically from a driver's `config.schema` (a JSON
//! Schema + editability tiers): the schema walker, the field kinds and the
//! coercion of a submission live in `rusty-photon-setup` (the drivers' own
//! setup pages render from the same walker); this module adds what only the
//! BFF has — the editability tiers, the hidden round-trip blob, the unlock
//! hatch and the htmx wiring. [`FieldModel`] wraps the walker's model with
//! the tiers; [`config_card`] renders any driver's form from it; [`merge_form`]
//! coerces a submission back into a `Config` using the same model. No
//! driver-specific field lists live here — every tier and field type comes
//! from the driver's own `config.schema` / `config.get`.
//!
//! The HTMX swap unit is the `#config-card` element: `GET /config/{service}`
//! returns the full page (or just the card for an HTMX request); `POST` and the
//! reconnect poll return a fresh `#config-card` fragment that HTMX swaps in by
//! `outerHTML`.

pub mod equipment;
pub mod stream;
pub mod targets;

use maud::{html, Markup, DOCTYPE};
use rusty_photon_setup::form::{array_contains, bool_at, overlay_field, text_at};
use rusty_photon_setup::schema::{resolve, unwrap_optional};
use rusty_photon_setup::{FieldKind, FieldSpec};
use serde_json::Value;

use crate::driver_client::{ConfigClientError, ConfigSchemaResponse, FieldError};

pub use rusty_photon_setup::form::canonical_json;
pub use rusty_photon_setup::schema::humanize;
pub use rusty_photon_setup::FormValues;

/// Page identity passed to the card/fragment renderers: the service id (used in
/// every route URL) plus the display strings shown in the card header.
pub struct Page<'a> {
    pub service: &'a str,
    pub title: &'a str,
    pub subtitle: &'a str,
    /// Whether the "Restart via Sentinel" affordances render: true only when
    /// the BFF has a `sentinel` block configured (the driver then has a
    /// Sentinel-side service name to target).
    pub can_restart: bool,
}

impl Page<'_> {
    /// The BFF route the restart affordances post to.
    fn restart_url(&self) -> String {
        format!("/config/{}/restart", self.service)
    }
}

/// A status banner rendered above the form.
#[derive(Debug, Clone)]
pub enum Banner {
    /// `config.apply` returned `status:"ok"` — persisted, no reload needed.
    Saved,
    /// `config.apply` returned `status:"ok"` with `restart_required[]` paths:
    /// persisted, but the listed changes only take effect on the target's next
    /// process start (`ApplyDisposition::Restart` — rp, which has no in-process
    /// reload — or a driver field classified restart-required). The restart
    /// callout; when a Sentinel is configured, the "Restart via Sentinel"
    /// affordance attaches inline.
    SavedRestartRequired(Vec<String>),
    /// `config.apply` returned `status:"invalid"`.
    Invalid,
    /// The reconnect poll found the driver back after a reload.
    Reconnected,
}

/// Which top-nav tab a page belongs to (highlighted as active in the shell).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NavTab {
    /// The activity stream (`/stream`).
    Activity,
    /// The equipment roster (`/equipment`).
    Equipment,
    /// The targets inbox (`/targets`).
    Targets,
    /// The config pages (`/`, `/config/{service}`).
    Configuration,
}

/// The full HTML shell: dark theme, embedded CSS + HTMX, and the top
/// nav.
///
/// The nav carries the four surface tabs plus the mock's pure-CSS
/// night-vision toggle (a page-level red filter via
/// `body:has(#night-vision:checked)`; no JavaScript).
#[must_use]
pub fn layout(title: &str, body: &Markup) -> Markup {
    layout_with_nav(title, NavTab::Configuration, body)
}

/// [`layout`] with an explicit active tab (the config-page shell defaults to
/// [`NavTab::Configuration`]).
#[must_use]
pub fn layout_with_nav(title: &str, active: NavTab, body: &Markup) -> Markup {
    html! {
        (DOCTYPE)
        html lang="en" {
            head {
                meta charset="utf-8";
                meta name="viewport" content="width=device-width, initial-scale=1";
                title { (title) }
                link rel="stylesheet" href="/assets/app.css";
                script src="/assets/htmx.min.js" {}
            }
            body {
                nav.topnav {
                    div.logo {}
                    span.title { "rusty-photon" }
                    div.nav-tabs {
                        a .active[active == NavTab::Activity] href="/stream" { "Activity" }
                        a .active[active == NavTab::Equipment] href="/equipment" { "Equipment" }
                        a .active[active == NavTab::Targets] href="/targets" { "Targets" }
                        a .active[active == NavTab::Configuration] href="/" { "Configuration" }
                    }
                    span.grow {}
                    label.night-toggle title="Toggle red night-vision mode" {
                        // Visually hidden, not `hidden`: the checkbox must stay
                        // focusable so the toggle works from the keyboard.
                        input type="checkbox" id="night-vision" class="visually-hidden";
                        span.nv-icon { "☾" }
                        span.nv-lbl { "Night vision" }
                        span.nv-dot {}
                    }
                }
                main.container { (body) }
            }
        }
    }
}

// --- the schema-derived field model -------------------------------------------

/// The driver's form model: the shared walker's fields plus the editability
/// tiers (both straight from `config.schema`). Drives both rendering and
/// `merge_form`.
///
/// Only the scalar leaves render and merge here: a leaf the walker names as
/// file-only, or a tagged enum, round-trips untouched through the hidden
/// blob, which is exactly how redacted secrets stay safe on this page.
pub struct FieldModel {
    inner: rusty_photon_setup::FieldModel,
    locked: Vec<String>,
    read_only: Vec<String>,
}

impl FieldModel {
    /// Build the model from a driver's `config.schema` response.
    #[must_use]
    pub fn from_schema(resp: &ConfigSchemaResponse) -> Self {
        Self {
            inner: rusty_photon_setup::FieldModel::from_schema(&resp.schema),
            locked: resp.locked_fields.clone(),
            read_only: resp.read_only_fields.clone(),
        }
    }

    /// Build the model for **one equipment entry** of `kind_key` from rp's
    /// config schema: navigate `properties.equipment.properties.{kind_key}`,
    /// step into `items` for the list kinds, unwrap the optional wrapper
    /// (`Option<MountConfig>` renders as an `anyOf` with a null branch), and
    /// walk that entry object as the root — field names come out relative
    /// (`alpaca_url`, not `equipment.cameras.0.alpaca_url`). The editability
    /// tiers don't apply inside an entry, so they are empty. `None` when the
    /// schema doesn't carry that kind (an rp older than the roster page).
    #[must_use]
    pub fn from_item_schema(resp: &ConfigSchemaResponse, kind_key: &str) -> Option<Self> {
        let root = &resp.schema;
        let equipment = resolve(root.get("properties")?.get("equipment")?, root);
        let kind_node = resolve(equipment.get("properties")?.get(kind_key)?, root);
        // A list kind reads the array's item schema; the singular mount
        // uses the property itself (optional-wrapped).
        let entry_node = kind_node
            .get("items")
            .map_or(kind_node, |items| resolve(items, root));
        let entry_node = unwrap_optional(entry_node, root)?;
        let inner = rusty_photon_setup::FieldModel::from_node(root, entry_node);
        if inner.fields().iter().all(|f| !f.is_editable_scalar()) {
            return None;
        }
        Some(Self {
            inner,
            locked: Vec::new(),
            read_only: Vec::new(),
        })
    }

    fn is_locked(&self, name: &str) -> bool {
        self.locked.iter().any(|n| n == name)
    }

    fn is_read_only(&self, name: &str) -> bool {
        self.read_only.iter().any(|n| n == name)
    }

    /// The ordered scalar leaves the page renders and merges — for renderers
    /// in sibling page modules.
    pub(crate) fn field_specs(&self) -> Vec<&FieldSpec> {
        self.inner
            .fields()
            .iter()
            .filter(|f| f.is_editable_scalar())
            .collect()
    }
}

// --- rendering ----------------------------------------------------------------

/// The per-render state every field is rendered against.
struct FieldCtx<'a> {
    model: &'a FieldModel,
    config: &'a Value,
    overrides: &'a [String],
    unlocked: &'a [String],
    errors: &'a [FieldError],
}

/// Render a driver's configuration form from its schema-derived [`FieldModel`],
/// filled from the effective config.
///
/// Override-pinned and hard-read-only fields render disabled;
/// locked/identity fields render disabled behind an "unlock to edit"
/// escape hatch unless listed in `unlocked`; `errors` annotate fields
/// after a rejected apply.
#[must_use]
pub fn config_card(
    page: &Page<'_>,
    model: &FieldModel,
    config: &Value,
    overrides: &[String],
    unlocked: &[String],
    errors: &[FieldError],
    banner: Option<Banner>,
) -> Markup {
    // Sorted-key serialization so the hidden blob's bytes are deterministic
    // regardless of serde_json's `preserve_order` feature. Key order is
    // semantically irrelevant to the round-trip, but it must be stable for
    // the cross-OS byte snapshots (P2).
    let config_blob = canonical_json(config);
    let overrides_blob = serde_json::to_string(overrides).unwrap_or_default();
    // The unlocked set round-trips on POST so an invalid submission re-renders
    // with the identity field still unlocked (rather than snapping shut and
    // discarding the user's in-progress edit).
    let unlocked_blob = serde_json::to_string(unlocked).unwrap_or_default();
    let ctx = FieldCtx {
        model,
        config,
        overrides,
        unlocked,
        errors,
    };
    let action = format!("/config/{}", page.service);
    let fields = model.field_specs();
    html! {
        div #config-card.card {
            @if let Some(b) = banner { (banner_markup(page, &b)) }
            h1 { (page.title) }
            p.subtitle { (page.subtitle) }
            // htmx-driven, JavaScript-required: the form submits via `hx-post`.
            // There is no non-JS `method`/`action` fallback — the UI requires htmx
            // (UI-testing plan §7; the genuine recovery path is ssh + edit the file).
            form hx-post=(action) hx-target="#config-card" hx-swap="outerHTML" {
                input type="hidden" name="__config" value=(config_blob);
                input type="hidden" name="__overrides" value=(overrides_blob);
                input type="hidden" name="__unlocked" value=(unlocked_blob);

                @for (section, fields) in group_by_section(&fields) {
                    fieldset {
                        legend { (rusty_photon_setup::schema::humanize(section)) }
                        @for spec in fields { (render_field(page, &ctx, spec)) }
                    }
                }
                div.actions { button.primary type="submit" { "Apply" } }
            }
            // The recovery hammer (config-actions plan Phase 4): Sentinel owns
            // process restart; this posts to the BFF's restart route, which
            // calls Sentinel's REST API. Outside the form so the POST carries
            // no form data, and rendered only when a Sentinel is configured.
            @if page.can_restart {
                div.card-footer {
                    div.hint {
                        (format!(
                            "Sentinel can restart {}'s process — the recovery hammer \
                             for a wedged service, and how saved changes that need a \
                             process restart take effect.",
                            page.service
                        ))
                    }
                    (restart_button(page))
                }
            }
        }
    }
}

/// The "Restart via Sentinel" htmx button (JS-required, plan §7). The native
/// `hx-confirm` prompt guards against a stray click bouncing a healthy driver.
fn restart_button(page: &Page<'_>) -> Markup {
    let confirm = format!(
        "Restart {}'s process via Sentinel? In-flight operations are lost.",
        page.service
    );
    html! {
        button.restart-sentinel type="button" hx-post=(page.restart_url())
            hx-target="#config-card" hx-swap="outerHTML" hx-confirm=(confirm) {
            "Restart via Sentinel"
        }
    }
}

/// Group fields by their top-level section, preserving first-seen order.
fn group_by_section<'a>(fields: &[&'a FieldSpec]) -> Vec<(&'a str, Vec<&'a FieldSpec>)> {
    let mut groups: Vec<(&str, Vec<&FieldSpec>)> = Vec::new();
    for f in fields {
        if let Some(group) = groups.iter_mut().find(|(s, _)| *s == f.section()) {
            group.1.push(f);
        } else {
            groups.push((f.section(), vec![f]));
        }
    }
    groups
}

/// Why a rendered field is disabled (or how its identity lock stands),
/// in the same priority order the hint text follows: a pinned field
/// stays pinned even if it is also read-only or locked.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum FieldGuard {
    /// Pinned by a command-line override.
    Pinned,
    /// Hard read-only.
    ReadOnly,
    /// Identity field, still locked.
    Locked,
    /// Identity field the operator unlocked this session.
    Unlocked,
    /// Freely editable.
    Editable,
}

impl FieldGuard {
    /// A locked/identity field is disabled until the user explicitly
    /// unlocks it; pinned and hard-read-only always disable regardless.
    const fn disables(self) -> bool {
        matches!(self, Self::Pinned | Self::ReadOnly | Self::Locked)
    }
}

fn render_field(page: &Page<'_>, ctx: &FieldCtx<'_>, spec: &FieldSpec) -> Markup {
    let name = spec.name();
    let guard = if ctx.overrides.iter().any(|o| o == name) {
        FieldGuard::Pinned
    } else if ctx.model.is_read_only(name) {
        FieldGuard::ReadOnly
    } else if ctx.model.is_locked(name) {
        if ctx.unlocked.iter().any(|u| u == name) {
            FieldGuard::Unlocked
        } else {
            FieldGuard::Locked
        }
    } else {
        FieldGuard::Editable
    };
    let disabled = guard.disables();
    let err = ctx.errors.iter().find(|e| e.path == name);
    html! {
        div.field.pinned[disabled].invalid[err.is_some()] {
            @match spec.kind() {
                FieldKind::Bool => {
                    div.checkbox {
                        input type="checkbox" id=(name) name=(name)
                            checked[bool_at(ctx.config, spec.pointer())] disabled[disabled];
                        label for=(name) { (spec.label()) }
                    }
                }
                // One checkbox per enumerated value, in schema order; all
                // share the field's `name`, so the checked values arrive as
                // repeated form pairs (see `FormValues`). The group heading
                // is a span (a bare `label` would reference no control);
                // `role="group"` + `aria-label` name the group for
                // assistive tech, and each member keeps its own real label.
                FieldKind::IntSet { options } => {
                    span.group-label { (spec.label()) }
                    div.checkbox-group role="group" aria-label=(spec.label()) {
                        @for option in options {
                            @let option_id = format!("{name}.{option}");
                            div.checkbox {
                                input type="checkbox" id=(option_id) name=(name) value=(option)
                                    checked[array_contains(ctx.config, spec.pointer(), *option)]
                                    disabled[disabled];
                                label for=(option_id) { (option) }
                            }
                        }
                    }
                }
                _ => {
                    label for=(name) { (spec.label()) }
                    input type=(spec.input_type()) id=(name) name=(name)
                        value=(text_at(ctx.config, spec.pointer())) disabled[disabled];
                }
            }
            @if let Some(e) = err { div.error { (e.msg) } }
            (field_hints(page, spec, guard))
        }
    }
}

fn field_hints(page: &Page<'_>, spec: &FieldSpec, guard: FieldGuard) -> Markup {
    let unlock_href = format!("/config/{}?unlock={}", page.service, spec.name());
    let lock_href = format!("/config/{}", page.service);
    html! {
        @if guard == FieldGuard::Pinned {
            div.hint { "Pinned by a command-line override; change the driver's launch flags to edit it." }
        } @else if guard == FieldGuard::ReadOnly {
            div.hint {
                "Read-only for now — editing it here would lose the connection to "
                "the driver. Change it in the driver's configuration file."
            }
        } @else if guard == FieldGuard::Locked {
            div.hint {
                "Identity — the driver owns this. Editing is an escape hatch "
                "for a misbehaving driver. "
                // A link-styled htmx button (JS-required, no `href` fallback —
                // plan §7). `type="button"` is load-bearing: this renders inside
                // the form, so a default-type button would submit it on click.
                button.link type="button" hx-get=(unlock_href)
                    hx-target="#config-card" hx-swap="outerHTML" { "Unlock to edit" }
            }
        } @else if guard == FieldGuard::Unlocked {
            div.hint.warning {
                "Unlocked — editing the driver's identity is an escape hatch "
                "for a misbehaving driver. "
                button.link type="button" hx-get=(lock_href)
                    hx-target="#config-card" hx-swap="outerHTML" { "Lock again" }
            }
        }
    }
}

/// The "applying — reconnecting" fragment: polls `…/status` once a second until
/// the driver answers and the poll swaps in a fresh card.
#[must_use]
pub fn reconnecting_card(service: &str) -> Markup {
    polling_card(service, "Saved — the driver is reloading. Reconnecting…")
}

/// The restart-accepted fragment: Sentinel restarted the driver's unit
/// through the platform service manager, so the driver's process is
/// coming back — same poll wiring as the reload flow.
///
/// `recovery_timed_out` adds that Sentinel's recovery check never
/// confirmed recovery within its budget (the poll may still succeed —
/// the budget is Sentinel's, not the driver's).
#[must_use]
pub fn restarting_card(service: &str, recovery_timed_out: bool) -> Markup {
    let message = if recovery_timed_out {
        "Restart requested via Sentinel, but its health check did not confirm \
         recovery within the budget. Reconnecting anyway…"
    } else {
        "Restart requested via Sentinel — the driver is restarting. Reconnecting…"
    };
    polling_card(service, message)
}

/// Shared skeleton of the reconnect-polling fragments: polls `…/status` once a
/// second until the driver answers and the poll swaps in a fresh card.
fn polling_card(service: &str, message: &str) -> Markup {
    let status_url = format!("/config/{service}/status");
    html! {
        div #config-card.card hx-get=(status_url) hx-trigger="every 1s"
            hx-swap="outerHTML" hx-target="this" {
            div class="banner applying" {
                span.dot {}
                span { (message) }
            }
        }
    }
}

/// An error card derived from a `ConfigClientError`, with a retry affordance.
#[must_use]
pub fn error_card(service: &str, err: &ConfigClientError) -> Markup {
    let message = if err.is_action_not_implemented() {
        "This driver does not expose configuration actions.".to_string()
    } else {
        err.to_string()
    };
    error_card_with_message(service, &message)
}

/// An error card with an explicit message (e.g. a malformed form submission).
#[must_use]
pub fn message_error_card(service: &str, message: &str) -> Markup {
    error_card_with_message(service, message)
}

/// An error card for a request that named a service the BFF doesn't know.
#[must_use]
pub fn unknown_service_card(service: &str) -> Markup {
    html! {
        div #config-card.card {
            div class="banner error" {
                span.dot {}
                span { (format!("No configured driver named \"{service}\".")) }
            }
            p { a href="/" { "Back to configuration" } }
        }
    }
}

/// The card for a `/config/{service}` key that resolved to no usable driver.
/// Each [`crate::ResolveError`] cause gets its own honest message — an
/// unreachable rp or an unusable roster entry is not "no such driver".
pub(crate) fn resolve_failure_card(service: &str, err: &crate::ResolveError) -> Markup {
    match err {
        crate::ResolveError::Unknown => unknown_service_card(service),
        crate::ResolveError::RpUnreachable(e) => error_card_with_message(
            service,
            &format!("Could not read rp's roster to resolve \"{service}\": {e}"),
        ),
        crate::ResolveError::BadRosterEntry(e) => html! {
            div #config-card.card {
                div class="banner error" {
                    span.dot {}
                    span { (format!("This device's roster entry can't be used: {e}")) }
                }
                p { a href="/equipment" { "Fix it on the Equipment page" } }
            }
        },
    }
}

fn error_card_with_message(service: &str, message: &str) -> Markup {
    let retry = format!("/config/{service}");
    html! {
        div #config-card.card {
            div class="banner error" { span.dot {} span { (message) } }
            p {
                // A link-styled htmx button (JS-required, no `href` fallback — §7).
                button.link type="button" hx-get=(retry) hx-target="#config-card"
                    hx-swap="outerHTML" { "Retry" }
            }
        }
    }
}

fn simple_banner(kind: &str, text: &str) -> Markup {
    html! {
        div class=(format!("banner {kind}")) { span.dot {} span { (text) } }
    }
}

fn banner_markup(page: &Page<'_>, banner: &Banner) -> Markup {
    match banner {
        // The restart callout lists the pending paths — persisted, but only in
        // effect after the target's next process start. When a Sentinel is
        // configured, the restart affordance attaches inline so the operator
        // can act on the callout directly.
        Banner::SavedRestartRequired(paths) => html! {
            div class="banner warn" {
                span.dot {}
                span {
                    (format!("Saved. These changes take effect when {} is restarted: ", page.service))
                    span.mono { (paths.join(", ")) }
                    @if page.can_restart { " " (restart_button(page)) }
                }
            }
        },
        Banner::Saved => simple_banner("ok", "Saved. No reload was needed."),
        Banner::Invalid => simple_banner(
            "error",
            "Some values were rejected. Fix the highlighted fields and apply again.",
        ),
        Banner::Reconnected => simple_banner(
            "ok",
            "Reconnected. The driver reloaded with the new configuration.",
        ),
    }
}

// --- form ⇆ config mapping -----------------------------------------------------

/// The merged config produced from a submitted form, ready to send to
/// `config.apply`, plus the override-pinned paths (echoed back so a re-render
/// keeps those fields disabled).
#[derive(Debug)]
pub struct MergedForm {
    pub config: Value,
    pub overrides: Vec<String>,
    /// The locked/identity fields that were unlocked on this submission (read
    /// back from the hidden `__unlocked` field). Echoed so a re-render after an
    /// invalid apply keeps them unlocked.
    pub unlocked: Vec<String>,
    /// BFF-side parse/range errors for numeric fields (e.g. a port above its
    /// schema maximum). When non-empty, the form is re-rendered with these field
    /// errors rather than sent to the driver.
    pub errors: Vec<FieldError>,
}

/// A malformed form submission (a missing or unparseable hidden field).
///
/// Both required hidden fields are always emitted by [`config_card`],
/// so their absence or corruption means the submission did not come
/// from a rendered page.
#[derive(Debug, thiserror::Error)]
pub enum FormError {
    #[error("the form was missing the hidden configuration field")]
    MissingConfig,
    #[error("the hidden configuration field was not valid JSON: {0}")]
    BadConfig(String),
    #[error("the form was missing the hidden overrides field")]
    MissingOverrides,
    #[error("the hidden overrides field was not valid JSON: {0}")]
    BadOverrides(String),
}

/// Rebuild the full Config from a submitted form: start from the hidden
/// round-tripped blob and overlay each editable schema leaf by JSON pointer.
///
/// Override-pinned, hard-read-only, and not-unlocked locked fields are
/// not overlaid (they round-trip from the blob); schema subtrees the
/// model does not render (file-only leaves, tagged enums, secrets)
/// round-trip untouched too.
///
/// # Errors
///
/// Returns a [`FormError`] if either required hidden field
/// (`__config`, `__overrides`) is missing or not valid JSON.
pub fn merge_form(form: &FormValues, model: &FieldModel) -> Result<MergedForm, FormError> {
    let raw = form.get("__config").ok_or(FormError::MissingConfig)?;
    let mut config: Value =
        serde_json::from_str(raw).map_err(|e| FormError::BadConfig(e.to_string()))?;
    // `__overrides` is required and validated like `__config`: a malformed value
    // would otherwise be silently treated as "no overrides", letting pinned
    // fields be overlaid on a re-render instead of surfacing the bad submission.
    let overrides_raw = form.get("__overrides").ok_or(FormError::MissingOverrides)?;
    let overrides: Vec<String> =
        serde_json::from_str(overrides_raw).map_err(|e| FormError::BadOverrides(e.to_string()))?;

    // `__unlocked` is optional; a missing or malformed value means "nothing
    // unlocked" (the safe default — locked fields stay read-only), and the set is
    // filtered to the schema's locked fields so a forged value can only ever
    // unlock a genuine identity field.
    let unlocked = unlocked_set_from_json(model, form.get("__unlocked"));

    let is_pinned = |name: &str| overrides.iter().any(|o| o == name);
    let is_unlocked = |name: &str| unlocked.iter().any(|u| u == name);

    let mut errors = Vec::new();
    for spec in model.field_specs() {
        if is_pinned(spec.name()) || model.is_read_only(spec.name()) {
            continue;
        }
        // A locked/identity field round-trips from the blob untouched unless the
        // user explicitly unlocked it (and it isn't pinned).
        if model.is_locked(spec.name()) && !is_unlocked(spec.name()) {
            continue;
        }
        overlay_field(&mut config, spec, form, &mut errors);
    }

    Ok(MergedForm {
        config,
        overrides,
        unlocked,
        errors,
    })
}

/// Parse a JSON string array of field names into the set of currently-unlocked
/// locked/identity fields, keeping only names that are actually locked in the
/// schema. `None`, empty input, or any parse failure yields an empty set.
fn unlocked_set_from_json(model: &FieldModel, raw: Option<&str>) -> Vec<String> {
    let Some(raw) = raw else {
        return Vec::new();
    };
    let names: Vec<String> = serde_json::from_str(raw).unwrap_or_default();
    names.into_iter().filter(|n| model.is_locked(n)).collect()
}

/// Compute the unlocked set from a `?unlock=<field>` query value. Only a name
/// that is actually a locked/identity field (per the schema) is honoured.
#[must_use]
pub fn unlocked_from_query(model: &FieldModel, unlock: Option<&str>) -> Vec<String> {
    match unlock {
        Some(name) if model.is_locked(name) => vec![name.to_string()],
        _ => Vec::new(),
    }
}

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
mod tests {
    use super::*;
    use serde_json::json;

    /// A representative dsd-fp2-shaped `config.schema` response: `$defs` + `$ref`
    /// (so the walker's ref-resolution is exercised), an `anyOf` optional struct
    /// (`server.auth`, file-only, never rendered here), a nullable optional int
    /// (`server.discovery_port`), and the three editability tiers the driver
    /// reports.
    fn sample_schema() -> ConfigSchemaResponse {
        ConfigSchemaResponse {
            schema: json!({
                "$defs": {
                    "AuthConfig": {
                        "type": "object",
                        "properties": { "password_hash": { "type": "string" } },
                    },
                    "SerialConfig": {
                        "type": "object",
                        "properties": {
                            "port": { "type": "string" },
                            "baud_rate": { "type": "integer", "format": "uint32", "minimum": 0 },
                            "polling_interval": { "type": "string" },
                            "timeout": { "type": "string" },
                        },
                    },
                    "ServerConfig": {
                        "type": "object",
                        "properties": {
                            "port": { "type": "integer", "format": "uint16", "minimum": 0, "maximum": 65535 },
                            "discovery_port": { "type": ["integer", "null"], "format": "uint16", "minimum": 0, "maximum": 65535 },
                            "auth": { "anyOf": [ { "$ref": "#/$defs/AuthConfig" }, { "type": "null" } ] },
                        },
                    },
                    "CoverCalibratorConfig": {
                        "type": "object",
                        "properties": {
                            "name": { "type": "string" },
                            "unique_id": { "type": "string" },
                            "description": { "type": "string" },
                            "enabled": { "type": "boolean" },
                            "max_brightness": { "type": "integer", "format": "uint32", "minimum": 0 },
                        },
                    },
                },
                "type": "object",
                "properties": {
                    "serial": { "$ref": "#/$defs/SerialConfig" },
                    "server": { "$ref": "#/$defs/ServerConfig" },
                    "cover_calibrator": { "$ref": "#/$defs/CoverCalibratorConfig" },
                },
            }),
            locked_fields: vec!["cover_calibrator.unique_id".to_string()],
            read_only_fields: vec![
                "server.port".to_string(),
                "cover_calibrator.enabled".to_string(),
            ],
        }
    }

    fn sample_model() -> FieldModel {
        FieldModel::from_schema(&sample_schema())
    }

    /// An rp-shaped config schema: `equipment` with an array kind (`cameras`,
    /// items behind a `$ref`) and the optional singular `mount`
    /// (`anyOf [$ref, null]` — the `Option<MountConfig>` shape schemars emits).
    fn rp_like_schema() -> ConfigSchemaResponse {
        ConfigSchemaResponse {
            schema: json!({
                "$defs": {
                    "CameraConfig": {
                        "type": "object",
                        "properties": {
                            "id": { "type": "string" },
                            "alpaca_url": { "type": "string" },
                            "device_number": { "type": "integer", "format": "uint32", "minimum": 0 },
                            "gain": { "type": "integer", "format": "uint32", "minimum": 0 },
                            // The integer-enum array shape (rp's dark-library
                            // cooler grid) — renders as a checkbox group.
                            "cooler_targets_c": {
                                "type": "array",
                                "items": { "type": "integer", "enum": [-15, -10, -5, 0, 5] },
                                "uniqueItems": true,
                            },
                            "auth": { "anyOf": [ { "$ref": "#/$defs/DeviceAuth" }, { "type": "null" } ] },
                        },
                    },
                    "DeviceAuth": {
                        "type": "object",
                        "properties": { "username": { "type": "string" }, "password": { "type": "string" } },
                    },
                    "MountConfig": {
                        "type": "object",
                        "properties": {
                            "alpaca_url": { "type": "string" },
                            "device_number": { "type": "integer", "format": "uint32", "minimum": 0 },
                        },
                    },
                    "EquipmentConfig": {
                        "type": "object",
                        "properties": {
                            "cameras": { "type": "array", "items": { "$ref": "#/$defs/CameraConfig" } },
                            "mount": { "anyOf": [ { "$ref": "#/$defs/MountConfig" }, { "type": "null" } ] },
                        },
                    },
                },
                "type": "object",
                "properties": {
                    "equipment": { "$ref": "#/$defs/EquipmentConfig" },
                    "server": { "type": "object", "properties": { "port": { "type": "integer" } } },
                },
            }),
            locked_fields: vec![],
            read_only_fields: vec!["server.port".to_string()],
        }
    }

    fn names(model: &FieldModel) -> Vec<&str> {
        model
            .field_specs()
            .into_iter()
            .map(FieldSpec::name)
            .collect()
    }

    #[test]
    fn from_item_schema_walks_an_array_kind_with_relative_names() {
        let model =
            FieldModel::from_item_schema(&rp_like_schema(), "cameras").expect("cameras item model");
        // Relative, sorted leaf names; the optional `auth` subtree is file-only
        // and so not rendered here, while the integer-enum array
        // (`cooler_targets_c`) is a renderable leaf.
        assert_eq!(
            names(&model),
            vec![
                "alpaca_url",
                "cooler_targets_c",
                "device_number",
                "gain",
                "id"
            ]
        );
        // Item models carry no tiers — the entry form has no read-only fields.
        assert_eq!(model.locked, Vec::<String>::new());
        assert_eq!(model.read_only, Vec::<String>::new());
    }

    #[test]
    fn the_page_never_renders_a_file_only_leaf_or_an_object_array() {
        // rp's `equipment.cameras` is an array of objects and `server.auth`
        // an optional struct — both keep round-tripping via the blob.
        let model = FieldModel::from_schema(&rp_like_schema());
        assert!(
            !names(&model).iter().any(|n| n.starts_with("equipment")),
            "object arrays must stay off the page: {:?}",
            names(&model)
        );
        let model = sample_model();
        assert!(
            !names(&model).iter().any(|n| n.starts_with("server.auth")),
            "server.auth must stay off the page: {:?}",
            names(&model)
        );
    }

    #[test]
    fn from_item_schema_unwraps_the_optional_singular_mount() {
        let model =
            FieldModel::from_item_schema(&rp_like_schema(), "mount").expect("mount item model");
        assert_eq!(names(&model), vec!["alpaca_url", "device_number"]);
    }

    #[test]
    fn from_item_schema_unknown_kind_is_none() {
        assert!(FieldModel::from_item_schema(&rp_like_schema(), "rotators").is_none());
        // A schema without an equipment block at all.
        let bare = ConfigSchemaResponse {
            schema: json!({ "type": "object", "properties": {} }),
            locked_fields: vec![],
            read_only_fields: vec![],
        };
        assert!(FieldModel::from_item_schema(&bare, "cameras").is_none());
    }

    /// A minimal schema with one integer-enum array leaf at a nested path,
    /// for the checkbox-group render test.
    fn grid_model() -> FieldModel {
        FieldModel::from_schema(&ConfigSchemaResponse {
            schema: json!({
                "type": "object",
                "properties": {
                    "cooling": {
                        "type": "object",
                        "properties": {
                            "targets": {
                                "type": "array",
                                "items": { "type": "integer", "enum": [-10, -5, 0] },
                                "uniqueItems": true,
                            },
                        },
                    },
                },
            }),
            locked_fields: vec![],
            read_only_fields: vec![],
        })
    }

    #[test]
    fn config_card_renders_a_checkbox_group_with_checked_members() {
        let model = grid_model();
        let config = json!({ "cooling": { "targets": [-10, 0] } });
        let markup = config_card(&page(), &model, &config, &[], &[], &[], None).into_string();
        // One checkbox per enum value, sharing the field name, with a
        // per-value id; the values present in the config render checked.
        for (id, value, checked) in [
            ("cooling.targets.-10", "-10", true),
            ("cooling.targets.-5", "-5", false),
            ("cooling.targets.0", "0", true),
        ] {
            let tag_start = format!(r#"id="{id}""#);
            let tag = markup
                .split("<input")
                .find(|frag| frag.contains(&tag_start))
                .unwrap_or_else(|| panic!("missing checkbox {id}:\n{markup}"));
            assert!(tag.contains(r#"name="cooling.targets""#), "{tag}");
            assert!(tag.contains(&format!(r#"value="{value}""#)), "{tag}");
            assert_eq!(
                tag.contains("checked"),
                checked,
                "checkbox {id} checked-state mismatch: {tag}"
            );
        }
    }

    fn sample_config() -> Value {
        json!({
            "serial": { "port": "/dev/ttyACM0", "baud_rate": 115_200, "polling_interval": "500ms", "timeout": "3s" },
            "server": { "port": 11119, "discovery_port": 32227, "tls": null, "auth": null },
            "cover_calibrator": { "name": "FP2", "unique_id": "dsd-fp2-001", "description": "panel", "enabled": true, "max_brightness": 4096 }
        })
    }

    fn page() -> Page<'static> {
        Page {
            service: "dsd-fp2",
            title: "Deep Sky Dad FP2",
            subtitle: "dsd-fp2 · covercalibrator",
            can_restart: false,
        }
    }

    fn card(unlocked: &[String], errors: &[FieldError], banner: Option<Banner>) -> String {
        let model = sample_model();
        config_card(
            &page(),
            &model,
            &sample_config(),
            &[],
            unlocked,
            errors,
            banner,
        )
        .into_string()
    }

    fn form_from(pairs: &[(&str, &str)]) -> FormValues {
        FormValues::from(
            pairs
                .iter()
                .map(|(k, v)| ((*k).to_string(), (*v).to_string()))
                .collect::<Vec<_>>(),
        )
    }

    /// The `<input ...>` tag whose `name` attribute is `name`.
    fn input_tag(markup: &str, name: &str) -> String {
        let pos = markup.find(&format!(r#"name="{name}""#)).unwrap();
        let start = markup
            .get(..pos)
            .and_then(|head| head.rfind("<input"))
            .unwrap();
        let tag = markup.get(start..).unwrap();
        let end = tag.find('>').unwrap();
        tag.get(..=end).unwrap().to_string()
    }

    // --- rendering -----------------------------------------------------------

    #[test]
    fn config_card_embeds_current_values_and_hidden_blob() {
        let markup = card(&[], &[], None);
        assert!(markup.contains(r#"value="/dev/ttyACM0""#), "{markup}");
        assert!(markup.contains(r#"value="4096""#), "{markup}");
        assert!(markup.contains(r#"name="__config""#), "{markup}");
        assert!(markup.contains(r#"name="__unlocked""#), "{markup}");
        // Form posts to the service-scoped route.
        assert!(markup.contains(r#"hx-post="/config/dsd-fp2""#), "{markup}");
        // The file-only `server.auth` leaf is not on the page at all: it
        // rides in the blob.
        assert!(!markup.contains(r#"id="server.auth""#), "{markup}");
    }

    #[test]
    fn config_card_disables_override_pinned_fields() {
        let model = sample_model();
        let overrides = vec!["serial.port".to_string()];
        let markup = config_card(
            &page(),
            &model,
            &sample_config(),
            &overrides,
            &[],
            &[],
            None,
        )
        .into_string();
        let tag = input_tag(&markup, "serial.port");
        assert!(tag.contains("disabled"), "serial.port not disabled: {tag}");
        assert!(
            markup.contains("Pinned by a command-line override"),
            "{markup}"
        );
    }

    #[test]
    fn config_card_shows_field_errors() {
        let errors = vec![FieldError {
            path: "serial.baud_rate".to_string(),
            msg: "must be greater than 0".to_string(),
        }];
        let markup = card(&[], &errors, Some(Banner::Invalid));
        assert!(markup.contains("must be greater than 0"), "{markup}");
        assert!(markup.contains("invalid"), "{markup}");
    }

    #[test]
    fn config_card_renders_enabled_read_only_via_schema_tier() {
        // `cover_calibrator.enabled` is in the schema's read_only_fields, so the
        // checkbox renders disabled — no hardcoded list needed.
        let markup = card(&[], &[], None);
        let tag = input_tag(&markup, "cover_calibrator.enabled");
        assert!(
            tag.contains("disabled"),
            "enabled checkbox not disabled: {tag}"
        );
        assert!(
            tag.contains(r#"type="checkbox""#),
            "enabled not a checkbox: {tag}"
        );
    }

    #[test]
    fn config_card_renders_server_port_read_only_via_schema_tier() {
        let markup = card(&[], &[], None);
        let tag = input_tag(&markup, "server.port");
        assert!(tag.contains("disabled"), "server.port not disabled: {tag}");
    }

    #[test]
    fn config_card_renders_unique_id_locked_by_default() {
        let markup = card(&[], &[], None);
        let tag = input_tag(&markup, "cover_calibrator.unique_id");
        assert!(tag.contains("disabled"), "unique_id not disabled: {tag}");
        assert!(
            markup.contains("Identity — the driver owns this"),
            "missing identity hint:\n{markup}"
        );
        assert!(
            markup.contains(r#"hx-get="/config/dsd-fp2?unlock=cover_calibrator.unique_id""#),
            "missing unlock link:\n{markup}"
        );
    }

    #[test]
    fn config_card_renders_unique_id_editable_when_unlocked() {
        let unlocked = vec!["cover_calibrator.unique_id".to_string()];
        let markup = card(&unlocked, &[], None);
        let tag = input_tag(&markup, "cover_calibrator.unique_id");
        assert!(!tag.contains("disabled"), "unique_id still disabled: {tag}");
        assert!(
            markup
                .contains(r#"name="__unlocked" value="[&quot;cover_calibrator.unique_id&quot;]""#),
            "missing/empty __unlocked hidden field:\n{markup}"
        );
        assert!(
            markup.contains("Lock again"),
            "missing lock-again link:\n{markup}"
        );
    }

    #[test]
    fn unlocked_from_query_only_honours_locked_fields() {
        let model = sample_model();
        assert_eq!(
            unlocked_from_query(&model, Some("cover_calibrator.unique_id")),
            vec!["cover_calibrator.unique_id".to_string()]
        );
        // A hard-read-only field, an editable field, a typo, or no query → nothing.
        assert_eq!(
            unlocked_from_query(&model, Some("server.port")),
            Vec::<String>::new()
        );
        assert_eq!(
            unlocked_from_query(&model, Some("serial.port")),
            Vec::<String>::new()
        );
        assert_eq!(
            unlocked_from_query(&model, Some("nonsense")),
            Vec::<String>::new()
        );
        assert_eq!(unlocked_from_query(&model, None), Vec::<String>::new());
    }

    #[test]
    fn reconnecting_card_polls_service_status() {
        let markup = reconnecting_card("dsd-fp2").into_string();
        assert!(
            markup.contains(r#"hx-get="/config/dsd-fp2/status""#),
            "{markup}"
        );
        assert!(markup.contains(r#"hx-trigger="every 1s""#), "{markup}");
    }

    #[test]
    fn error_card_explains_action_not_implemented() {
        let err = ConfigClientError::Ascom {
            code: crate::driver_client::ACTION_NOT_IMPLEMENTED,
            message: "nope".to_string(),
        };
        let markup = error_card("dsd-fp2", &err).into_string();
        assert!(
            markup.contains("does not expose configuration actions"),
            "{markup}"
        );
    }

    // --- merge_form ----------------------------------------------------------

    #[test]
    fn merge_form_overlays_editable_fields() {
        let form = form_from(&[
            ("__config", &sample_config().to_string()),
            ("__overrides", "[]"),
            ("serial.port", "/dev/ttyACM5"),
            ("cover_calibrator.max_brightness", "2048"),
        ]);
        let merged = merge_form(&form, &sample_model()).unwrap();
        assert_eq!(
            merged
                .config
                .pointer("/serial/port")
                .and_then(Value::as_str),
            Some("/dev/ttyACM5")
        );
        assert_eq!(
            merged
                .config
                .pointer("/cover_calibrator/max_brightness")
                .and_then(Value::as_u64),
            Some(2048)
        );
        assert!(merged.errors.is_empty(), "{:?}", merged.errors);
    }

    #[test]
    fn merge_form_reports_a_coercion_error_and_keeps_the_prior_value() {
        // 99999 doesn't fit the schema maximum (65535) for discovery_port.
        let form = form_from(&[
            ("__config", &sample_config().to_string()),
            ("__overrides", "[]"),
            ("server.discovery_port", "99999"),
        ]);
        let merged = merge_form(&form, &sample_model()).unwrap();
        assert_eq!(merged.errors.len(), 1, "{:?}", merged.errors);
        assert_eq!(merged.errors[0].path, "server.discovery_port");
        assert_eq!(
            merged
                .config
                .pointer("/server/discovery_port")
                .and_then(Value::as_u64),
            Some(32227)
        );
    }

    #[test]
    fn merge_form_does_not_overlay_pinned_fields() {
        let form = form_from(&[
            ("__config", &sample_config().to_string()),
            ("__overrides", r#"["serial.port"]"#),
            ("serial.port", "/dev/ttyACM9"),
        ]);
        let merged = merge_form(&form, &sample_model()).unwrap();
        assert_eq!(
            merged
                .config
                .pointer("/serial/port")
                .and_then(Value::as_str),
            Some("/dev/ttyACM0")
        );
        assert_eq!(merged.overrides, vec!["serial.port".to_string()]);
    }

    #[test]
    fn merge_form_never_changes_read_only_fields() {
        // `enabled` and `server.port` are read-only via the schema tier, so a
        // forged value is ignored and the field round-trips from the blob.
        let form = form_from(&[
            ("__config", &sample_config().to_string()),
            ("__overrides", "[]"),
            ("cover_calibrator.enabled", "false"),
            ("server.port", "22222"),
        ]);
        let merged = merge_form(&form, &sample_model()).unwrap();
        assert_eq!(
            merged
                .config
                .pointer("/cover_calibrator/enabled")
                .and_then(Value::as_bool),
            Some(true)
        );
        assert_eq!(
            merged
                .config
                .pointer("/server/port")
                .and_then(Value::as_u64),
            Some(11119)
        );
    }

    #[test]
    fn merge_form_missing_blob_is_an_error() {
        let form = form_from(&[("serial.port", "/dev/ttyACM0")]);
        let err = merge_form(&form, &sample_model()).unwrap_err();
        assert!(matches!(err, FormError::MissingConfig));
    }

    #[test]
    fn merge_form_ignores_unique_id_when_not_unlocked() {
        let form = form_from(&[
            ("__config", &sample_config().to_string()),
            ("__overrides", "[]"),
            ("cover_calibrator.unique_id", "tampered-id"),
        ]);
        let merged = merge_form(&form, &sample_model()).unwrap();
        assert_eq!(
            merged
                .config
                .pointer("/cover_calibrator/unique_id")
                .and_then(Value::as_str),
            Some("dsd-fp2-001")
        );
        assert!(merged.unlocked.is_empty(), "{:?}", merged.unlocked);
    }

    #[test]
    fn merge_form_overlays_unique_id_when_unlocked() {
        let form = form_from(&[
            ("__config", &sample_config().to_string()),
            ("__overrides", "[]"),
            ("__unlocked", r#"["cover_calibrator.unique_id"]"#),
            ("cover_calibrator.unique_id", "fixed-by-operator"),
        ]);
        let merged = merge_form(&form, &sample_model()).unwrap();
        assert_eq!(
            merged
                .config
                .pointer("/cover_calibrator/unique_id")
                .and_then(Value::as_str),
            Some("fixed-by-operator")
        );
        assert_eq!(
            merged.unlocked,
            vec!["cover_calibrator.unique_id".to_string()]
        );
    }

    #[test]
    fn merge_form_pinned_unique_id_wins_over_unlock() {
        let form = form_from(&[
            ("__config", &sample_config().to_string()),
            ("__overrides", r#"["cover_calibrator.unique_id"]"#),
            ("__unlocked", r#"["cover_calibrator.unique_id"]"#),
            ("cover_calibrator.unique_id", "tampered-id"),
        ]);
        let merged = merge_form(&form, &sample_model()).unwrap();
        assert_eq!(
            merged
                .config
                .pointer("/cover_calibrator/unique_id")
                .and_then(Value::as_str),
            Some("dsd-fp2-001")
        );
    }

    #[test]
    fn merge_form_forged_unlocked_cannot_unlock_non_locked_field() {
        let form = form_from(&[
            ("__config", &sample_config().to_string()),
            ("__overrides", "[]"),
            ("__unlocked", r#"["server.port"]"#),
            ("server.port", "22222"),
        ]);
        let merged = merge_form(&form, &sample_model()).unwrap();
        assert_eq!(
            merged
                .config
                .pointer("/server/port")
                .and_then(Value::as_u64),
            Some(11119)
        );
        assert!(merged.unlocked.is_empty(), "{:?}", merged.unlocked);
    }

    #[test]
    fn merge_form_malformed_unlocked_is_treated_as_empty() {
        let form = form_from(&[
            ("__config", &sample_config().to_string()),
            ("__overrides", "[]"),
            ("__unlocked", "not json"),
            ("cover_calibrator.unique_id", "tampered-id"),
        ]);
        let merged = merge_form(&form, &sample_model()).unwrap();
        assert_eq!(
            merged
                .config
                .pointer("/cover_calibrator/unique_id")
                .and_then(Value::as_str),
            Some("dsd-fp2-001")
        );
        assert!(merged.unlocked.is_empty(), "{:?}", merged.unlocked);
    }

    #[test]
    fn merge_form_missing_overrides_is_an_error() {
        let form = form_from(&[("__config", &sample_config().to_string())]);
        let err = merge_form(&form, &sample_model()).unwrap_err();
        assert!(matches!(err, FormError::MissingOverrides), "{err:?}");
    }

    #[test]
    fn merge_form_invalid_overrides_is_an_error() {
        let form = form_from(&[
            ("__config", &sample_config().to_string()),
            ("__overrides", "not json"),
        ]);
        let err = merge_form(&form, &sample_model()).unwrap_err();
        assert!(matches!(err, FormError::BadOverrides(_)), "{err:?}");
    }
}
